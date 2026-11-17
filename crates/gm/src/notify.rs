//! Outbound notifications behind a channel interface (ADR-0006 as amended): Telegram now
//! ([`crate::telegram::TelegramClient`]), others later. Content stays literal text (invariant 9). Trusted formatting uses explicit spans, so Google
//! content is never parsed as markup.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

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

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntityKind {
    Bold,
    Strikethrough,
    TextLink { url: String },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MessageEntity {
    /// Telegram measures both positions in UTF-16 code units, including emoji surrogates.
    pub offset: usize,
    pub length: usize,
    #[serde(flatten)]
    pub kind: EntityKind,
}

/// Text with app-owned formatting spans, built left to right. Every Telegram message with
/// formatting (mail and calendar notifications) is built here, so offsets are always UTF-16 and a
/// cut at [`TELEGRAM_LIMIT`] never leaves a span past the end.
#[derive(Debug, Default)]
pub struct RichText {
    text: String,
    entities: Vec<MessageEntity>,
}

/// A finished [`RichText`]: what [`OutgoingMessage`] sends, and what the web renders as segments.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub text: String,
    pub entities: Vec<MessageEntity>,
}

impl RichText {
    pub fn push(&mut self, text: &str) {
        self.text.push_str(text);
    }

    pub fn styled(&mut self, text: &str, kind: EntityKind) {
        let offset = self.text.encode_utf16().count();
        self.push(text);
        let length = text.encode_utf16().count();
        if length > 0 {
            self.entities.push(MessageEntity {
                offset,
                length,
                kind,
            });
        }
    }

    pub fn bold(&mut self, text: &str) {
        self.styled(text, EntityKind::Bold);
    }

    pub fn link(&mut self, text: &str, url: &str) {
        self.styled(
            text,
            EntityKind::TextLink {
                url: url.to_string(),
            },
        );
    }

    /// Cut to [`TELEGRAM_LIMIT`] UTF-16 units (with "…") and trim trailing whitespace; spans are
    /// clipped to the text that is left.
    pub fn finish(mut self) -> Rendered {
        if self.text.encode_utf16().count() > TELEGRAM_LIMIT {
            let mut units = 0;
            let end = self
                .text
                .char_indices()
                .find_map(|(byte, ch)| {
                    if units + ch.len_utf16() > TELEGRAM_LIMIT - 1 {
                        return Some(byte);
                    }
                    units += ch.len_utf16();
                    None
                })
                .unwrap_or(self.text.len());
            self.text.truncate(end);
            self.text.push('…');
            self.entities.retain_mut(|entity| {
                if entity.offset >= units {
                    return false;
                }
                entity.length = entity.length.min(units - entity.offset);
                true
            });
        }
        Rendered {
            text: self.text.trim_end().to_string(),
            entities: self.entities,
        }
    }
}

/// A run of [`Rendered`] text with the formatting of the spans that cover it, for the web page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Segment {
    pub text: String,
    pub bold: bool,
    pub strike: bool,
    /// Link target; empty for plain text.
    pub url: String,
}

impl Rendered {
    /// Split the text where spans start and end. Spans that do not fall on character boundaries
    /// are ignored.
    pub fn segments(&self) -> Vec<Segment> {
        let units: Vec<u16> = self.text.encode_utf16().collect();
        let mut cuts = vec![0, units.len()];
        for e in &self.entities {
            cuts.extend([
                e.offset.min(units.len()),
                (e.offset + e.length).min(units.len()),
            ]);
        }
        cuts.sort_unstable();
        cuts.dedup();
        cuts.windows(2)
            .filter_map(|w| {
                let text = String::from_utf16(&units[w[0]..w[1]]).ok()?;
                let mut seg = Segment {
                    text,
                    ..Default::default()
                };
                for e in &self.entities {
                    if e.offset <= w[0] && w[1] <= e.offset + e.length {
                        match &e.kind {
                            EntityKind::Bold => seg.bold = true,
                            EntityKind::Strikethrough => seg.strike = true,
                            EntityKind::TextLink { url } => seg.url = url.clone(),
                        }
                    }
                }
                Some(seg)
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutgoingMessage {
    pub text: String,
    /// App-owned formatting spans. Message content is never parsed as HTML or Markdown.
    pub entities: Vec<MessageEntity>,
    /// Deliver without sound (Telegram `disable_notification`).
    pub silent: bool,
    /// Inline keyboard rows; empty for none.
    pub buttons: Keyboard,
    /// Send as a reply to this message of the same chat (Telegram `reply_parameters`).
    pub reply_to: Option<i64>,
}

impl OutgoingMessage {
    pub fn text(text: impl Into<String>, silent: bool) -> Self {
        Self {
            text: text.into(),
            entities: vec![],
            silent,
            buttons: vec![],
            reply_to: None,
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

/// Build the notification for one mail event. `data` is `events.data` (sender, subject, link,
/// verdict); `summary` is `events.summary`. Suggestions are one bullet each; a suggestion with a
/// trusted link is a text link followed by the link's host. The Gmail link (built by code) is the
/// last line, as an "Open in Gmail" text link.
pub fn format_email_notification(
    tr: &Tr,
    data: &Value,
    summary: Option<&Value>,
    priority: Priority,
) -> Rendered {
    let subject = data["subject"].as_str().unwrap_or("(no subject)");
    let sender = data["from"].as_str().unwrap_or("?");
    let link = data["link"].as_str().unwrap_or_default();
    let mut m = RichText::default();

    if let Some(message) = data["verdict"]["message"].as_str() {
        m.push(&format!("{} {message}\n", icon(priority, None)));
        m.push(&format!("{}: {sender}\n", tr.get("notif-from")));
    } else {
        m.push(&format!("{} 📧 {subject}\n", icon(priority, summary)));
        m.push(&format!("{}: {sender}\n", tr.get("notif-from")));
        if let Some(s) = summary.filter(|s| s["summary"].is_string()) {
            m.push(&format!(
                "\n{}\n",
                s["summary"].as_str().unwrap_or_default().trim()
            ));
            let actions = crate::llm::SuggestedAction::list(s);
            if !actions.is_empty() {
                m.push(&format!("\n{}:\n", tr.get("notif-suggested")));
                for a in actions {
                    m.push("• ");
                    match a.host() {
                        Some(host) => {
                            m.link(a.text.trim(), &a.url);
                            m.push(&format!(" ({host})\n"));
                        }
                        None => m.push(&format!("{}\n", a.text.trim())),
                    }
                }
            }
        }
    }
    if !link.is_empty() {
        m.push("\n");
        m.link(&tr.get("notif-open-gmail"), link);
    }
    m.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_follow_spans_measured_in_utf16() {
        let mut m = RichText::default();
        m.push("🚀 ");
        m.bold("Title");
        m.push("\n");
        m.link("Open", "https://calendar.google.com/calendar/x");
        let segs = m.finish().segments();
        let got: Vec<(&str, bool, &str)> = segs
            .iter()
            .map(|s| (s.text.as_str(), s.bold, s.url.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("🚀 ", false, ""),
                ("Title", true, ""),
                ("\n", false, ""),
                ("Open", false, "https://calendar.google.com/calendar/x"),
            ]
        );
    }
}
