//! Outbound notifications behind a channel interface (ADR-0006 as amended): Telegram now, others
//! later. Messages are plain text (invariant 8): mail-derived text is never sent as markup.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::i18n::Tr;
use crate::rules::Priority;

pub const TELEGRAM_LIMIT: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub struct OutgoingMessage {
    pub text: String,
    /// Deliver without sound (Telegram `disable_notification`).
    pub silent: bool,
}

#[async_trait]
pub trait Notifier: Send + Sync {
    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()>;
    fn name(&self) -> &'static str;
}

pub struct TelegramNotifier {
    http: reqwest::Client,
    api_base: String,
    token: String,
    chat_id: String,
}

impl TelegramNotifier {
    pub fn new(http: reqwest::Client, api_base: &str, token: &str, chat_id: &str) -> Self {
        Self {
            http,
            api_base: api_base.trim_end_matches('/').into(),
            token: token.into(),
            chat_id: chat_id.into(),
        }
    }
}

#[async_trait]
impl Notifier for TelegramNotifier {
    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()> {
        let resp = self
            .http
            .post(format!("{}/bot{}/sendMessage", self.api_base, self.token))
            .json(&json!({
                "chat_id": self.chat_id,
                "text": msg.text,
                "disable_notification": msg.silent,
                "link_preview_options": {"is_disabled": true},
            }))
            .send()
            .await
            // Never log the URL: it contains the bot token (invariant 5).
            .map_err(|e| anyhow::anyhow!("telegram request failed: {}", e.without_url()))?;
        let status = resp.status();
        if !status.is_success() {
            let body: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            anyhow::bail!("telegram sendMessage failed: {status} {body}");
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "telegram"
    }
}

/// Used when Telegram is not configured: notifications only go to the log.
pub struct LogNotifier;

#[async_trait]
impl Notifier for LogNotifier {
    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()> {
        tracing::info!(silent = msg.silent, text = %msg.text, "NOTIFICATION");
        Ok(())
    }

    fn name(&self) -> &'static str {
        "log"
    }
}

/// Keeps sent messages in memory (tests and dry runs).
#[derive(Default)]
pub struct RecordingNotifier {
    pub sent: Mutex<Vec<OutgoingMessage>>,
}

#[async_trait]
impl Notifier for RecordingNotifier {
    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(msg.clone());
        Ok(())
    }

    fn name(&self) -> &'static str {
        "recording"
    }
}

fn icon(priority: Priority, summary: Option<&Value>) -> &'static str {
    match priority {
        Priority::Urgent => "🚨",
        Priority::Silent => "🔕",
        Priority::High => "🔴",
        Priority::Normal => match summary.and_then(|s| s["importance"].as_str()) {
            Some("high") => "🔴",
            Some("low") => "⚪",
            _ => "🟡",
        },
    }
}

fn truncate(text: String) -> String {
    if text.chars().count() <= TELEGRAM_LIMIT {
        return text;
    }
    let mut t: String = text.chars().take(TELEGRAM_LIMIT - 1).collect();
    t.push('…');
    t
}

/// Build the notification for one mail event. `data` is `events.data` (sender, subject, link,
/// verdict); `summary` is `events.summary`.
pub fn format_email_notification(
    tr: &Tr,
    data: &Value,
    summary: Option<&Value>,
    priority: Priority,
) -> String {
    let subject = data["subject"].as_str().unwrap_or("(no subject)");
    let sender = data["from"].as_str().unwrap_or("?");
    let link = data["link"].as_str().unwrap_or_default();
    let rule_message = data["verdict"]["message"].as_str();
    let mut lines = Vec::new();

    if let Some(message) = rule_message {
        lines.push(format!("{} {message}", icon(priority, None)));
        lines.push(format!("{}: {sender}", tr.get("notif-from")));
    } else {
        lines.push(format!("{} 📧 {subject}", icon(priority, summary)));
        lines.push(format!("{}: {sender}", tr.get("notif-from")));
        match summary {
            Some(s) if s["summary"].is_string() => {
                lines.push(String::new());
                lines.push(s["summary"].as_str().unwrap_or_default().trim().to_string());
                let actions: Vec<&str> = s["suggested_actions"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if !actions.is_empty() {
                    lines.push(String::new());
                    lines.push(format!(
                        "{}: {}",
                        tr.get("notif-suggested"),
                        actions.join(" · ")
                    ));
                }
            }
            _ => {}
        }
    }
    if !link.is_empty() {
        lines.push(String::new());
        lines.push(link.to_string());
    }
    // Remove a trailing empty line left by rule-only messages.
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    truncate(lines.join("\n"))
}
