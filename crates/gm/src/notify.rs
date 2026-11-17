//! Outbound notifications behind a channel interface (ADR-0006 as amended): Telegram now, others
//! later. Messages are plain text (invariant 8): mail-derived text is never sent as markup.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::i18n::Tr;
use crate::rules::Priority;

pub const TELEGRAM_LIMIT: usize = 4096;

/// An inline button: `data` comes back in a Telegram `callback_query` (at most 64 bytes).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Button {
    pub text: String,
    pub data: String,
}

pub type Keyboard = Vec<Vec<Button>>;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutgoingMessage {
    pub text: String,
    /// Deliver without sound (Telegram `disable_notification`).
    pub silent: bool,
    /// Inline keyboard rows; empty for none.
    pub buttons: Keyboard,
}

impl OutgoingMessage {
    pub fn text(text: impl Into<String>, silent: bool) -> Self {
        Self {
            text: text.into(),
            silent,
            buttons: vec![],
        }
    }
}

#[async_trait]
pub trait Notifier: Send + Sync {
    /// Send a message; returns the channel's message id when it has one (needed to edit later).
    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<Option<i64>>;
    /// Replace the text and keyboard of a sent message.
    async fn edit(&self, _message_id: i64, _msg: &OutgoingMessage) -> anyhow::Result<()> {
        Ok(())
    }
    /// Replace only the keyboard of a sent message.
    async fn edit_buttons(&self, _message_id: i64, _buttons: &Keyboard) -> anyhow::Result<()> {
        Ok(())
    }
    /// Acknowledge a button press (shows a short toast).
    async fn answer_callback(&self, _callback_id: &str, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn name(&self) -> &'static str;
}

pub struct TelegramNotifier {
    http: reqwest::Client,
    api_base: String,
    token: String,
    chat_id: String,
}

fn keyboard_json(buttons: &Keyboard) -> Value {
    json!({"inline_keyboard": buttons.iter().map(|row| row.iter().map(|b| json!({"text": b.text, "callback_data": b.data})).collect::<Vec<_>>()).collect::<Vec<_>>()})
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

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    /// Call a Bot API method. Never logs or returns the URL: it contains the bot token
    /// (invariant 5). "message is not modified" counts as success (edits are idempotent).
    pub async fn call(&self, method: &str, body: &Value) -> anyhow::Result<Value> {
        let resp = self
            .http
            .post(format!("{}/bot{}/{method}", self.api_base, self.token))
            .json(body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("telegram {method} failed: {}", e.without_url()))?;
        let status = resp.status();
        if !status.is_success() {
            let body: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            if status.as_u16() == 400 && body.contains("message is not modified") {
                return Ok(Value::Null);
            }
            anyhow::bail!("telegram {method} failed: {status} {body}");
        }
        resp.json::<Value>()
            .await
            .map_err(|e| anyhow::anyhow!("telegram {method}: bad response: {}", e.without_url()))
    }
}

#[async_trait]
impl Notifier for TelegramNotifier {
    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<Option<i64>> {
        let mut body = json!({
            "chat_id": self.chat_id,
            "text": msg.text,
            "disable_notification": msg.silent,
            "link_preview_options": {"is_disabled": true},
        });
        if !msg.buttons.is_empty() {
            body["reply_markup"] = keyboard_json(&msg.buttons);
        }
        let r = self.call("sendMessage", &body).await?;
        Ok(r["result"]["message_id"].as_i64())
    }

    async fn edit(&self, message_id: i64, msg: &OutgoingMessage) -> anyhow::Result<()> {
        let mut body = json!({
            "chat_id": self.chat_id,
            "message_id": message_id,
            "text": msg.text,
            "link_preview_options": {"is_disabled": true},
        });
        if !msg.buttons.is_empty() {
            body["reply_markup"] = keyboard_json(&msg.buttons);
        }
        self.call("editMessageText", &body).await?;
        Ok(())
    }

    async fn edit_buttons(&self, message_id: i64, buttons: &Keyboard) -> anyhow::Result<()> {
        self.call(
            "editMessageReplyMarkup",
            &json!({"chat_id": self.chat_id, "message_id": message_id, "reply_markup": keyboard_json(buttons)}),
        )
        .await?;
        Ok(())
    }

    async fn answer_callback(&self, callback_id: &str, text: &str) -> anyhow::Result<()> {
        self.call(
            "answerCallbackQuery",
            &json!({"callback_query_id": callback_id, "text": text}),
        )
        .await?;
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
    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<Option<i64>> {
        tracing::info!(silent = msg.silent, text = %msg.text, "NOTIFICATION");
        Ok(None)
    }

    fn name(&self) -> &'static str {
        "log"
    }
}

/// Keeps sent messages in memory (tests and dry runs). Message ids are 1, 2, 3… in send order.
#[derive(Default)]
pub struct RecordingNotifier {
    pub sent: Mutex<Vec<OutgoingMessage>>,
    pub edits: Mutex<Vec<(i64, OutgoingMessage)>>,
    pub button_edits: Mutex<Vec<(i64, Keyboard)>>,
    pub answers: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl Notifier for RecordingNotifier {
    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<Option<i64>> {
        let mut sent = self.sent.lock().unwrap();
        sent.push(msg.clone());
        Ok(Some(sent.len() as i64))
    }

    async fn edit(&self, message_id: i64, msg: &OutgoingMessage) -> anyhow::Result<()> {
        self.edits.lock().unwrap().push((message_id, msg.clone()));
        Ok(())
    }

    async fn edit_buttons(&self, message_id: i64, buttons: &Keyboard) -> anyhow::Result<()> {
        self.button_edits
            .lock()
            .unwrap()
            .push((message_id, buttons.clone()));
        Ok(())
    }

    async fn answer_callback(&self, callback_id: &str, text: &str) -> anyhow::Result<()> {
        self.answers
            .lock()
            .unwrap()
            .push((callback_id.into(), text.into()));
        Ok(())
    }

    fn name(&self) -> &'static str {
        "recording"
    }
}

/// Callback data of a feedback button: `f:<event_id>:up|down`.
pub fn feedback_data(event_id: i64, label: &str) -> String {
    format!("f:{event_id}:{label}")
}

/// A 👍 / 👎 row for one event; `suffix` numbers digest items (" 1"); the current choice gets ✅.
pub fn feedback_row(event_id: i64, suffix: &str, chosen: Option<&str>) -> Vec<Button> {
    [("up", "👍"), ("down", "👎")]
        .into_iter()
        .map(|(label, icon)| Button {
            text: format!(
                "{}{icon}{suffix}",
                if chosen == Some(label) { "✅" } else { "" }
            ),
            data: feedback_data(event_id, label),
        })
        .collect()
}

/// Split `text` into Telegram-sized parts at line boundaries (a single over-long line is cut).
pub fn split_message(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        let line: String = if line.chars().count() > TELEGRAM_LIMIT {
            line.chars().take(TELEGRAM_LIMIT - 1).collect::<String>() + "…"
        } else {
            line.to_string()
        };
        let extra = if cur.is_empty() { 0 } else { 1 };
        if cur.chars().count() + extra + line.chars().count() > TELEGRAM_LIMIT {
            parts.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(&line);
    }
    if !cur.is_empty() || parts.is_empty() {
        parts.push(cur);
    }
    parts
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
