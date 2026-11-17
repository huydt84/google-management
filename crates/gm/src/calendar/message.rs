//! Calendar message hierarchy and Telegram-native formatting. Google text remains literal text.

use serde_json::Value;

use crate::i18n::Tr;
use crate::notify::{EntityKind, MessageEntity, TELEGRAM_LIMIT};

pub(crate) struct RenderedNotification {
    pub text: String,
    pub entities: Vec<MessageEntity>,
}

#[derive(Default)]
struct Message {
    text: String,
    entities: Vec<MessageEntity>,
}

impl Message {
    fn push(&mut self, text: &str) {
        self.text.push_str(text);
    }

    fn styled(&mut self, text: &str, kind: EntityKind) {
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

    fn bold(&mut self, text: &str) {
        self.styled(text, EntityKind::Bold);
    }

    fn field(&mut self, label: &str, value: &str) {
        self.bold(label);
        self.push(": ");
        self.push(value);
        self.push("\n");
    }

    fn change(&mut self, label: &str, old: &str, new: &str) {
        if old == new {
            return;
        }
        self.push("\n");
        self.bold(label);
        self.push("\n");
        self.styled(old, EntityKind::Strikethrough);
        self.push("\n→ ");
        self.bold(new);
        self.push("\n");
    }

    fn finish(mut self) -> RenderedNotification {
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
        RenderedNotification {
            text: self.text.trim_end().to_string(),
            entities: self.entities,
        }
    }
}

fn value(text: Option<&str>, limit: usize) -> String {
    text.unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

fn organizer(tr: &Tr, event: &Value) -> String {
    if event["organizer"]["self"] == true {
        return tr.get("calendar-you");
    }
    let name = value(
        event["organizer"]["displayName"]
            .as_str()
            .or(event["organizer"]["email"].as_str()),
        300,
    );
    if name.is_empty() {
        tr.get("calendar-unset")
    } else {
        name
    }
}

fn location(tr: &Tr, event: &Value) -> String {
    let text = value(event["location"].as_str(), 500);
    if text.is_empty() {
        tr.get("calendar-unset")
    } else {
        text
    }
}

fn calendar_link(event: &Value) -> Option<String> {
    let url = url::Url::parse(event["htmlLink"].as_str()?).ok()?;
    (url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("calendar.google.com" | "www.google.com")
        )
        && url.path().starts_with("/calendar/"))
    .then(|| url.to_string())
}

pub(crate) fn notification(tr: &Tr, kind: &str, data: &Value) -> RenderedNotification {
    let event = &data["event"];
    let previous = &data["previous"];
    let (icon, heading) = match kind {
        "event.invitation" => ("📨", "calendar-invitation"),
        "event.cancelled" => ("❌", "calendar-cancelled"),
        _ if !previous.is_object() => ("📅", "calendar-created"),
        _ => ("✏️", "calendar-changed"),
    };
    let mut message = Message::default();
    message.push(&format!("{icon} "));
    message.bold(&tr.get(heading));
    message.push("\n");
    message.bold(&super::title(tr, event));
    message.push("\n\n");
    message.field(&tr.get("calendar-when"), &super::time_range(tr, event));
    message.field(&tr.get("calendar-organizer"), &organizer(tr, event));
    if event["location"]
        .as_str()
        .is_some_and(|s| !s.trim().is_empty())
    {
        message.field(&tr.get("calendar-field-location"), &location(tr, event));
    }
    if kind == "event.changed" && previous.is_object() {
        let changed = |key: &str| {
            data["changes"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == key))
        };
        if changed("summary") {
            message.change(
                &tr.get("calendar-field-summary"),
                &super::title(tr, previous),
                &super::title(tr, event),
            );
        }
        if changed("start") || changed("end") {
            message.change(
                &tr.get("calendar-field-start"),
                &super::time_range(tr, previous),
                &super::time_range(tr, event),
            );
        }
        if changed("location") {
            message.change(
                &tr.get("calendar-field-location"),
                &location(tr, previous),
                &location(tr, event),
            );
        }
        if changed("organizer") {
            message.change(
                &tr.get("calendar-organizer"),
                &organizer(tr, previous),
                &organizer(tr, event),
            );
        }
        for key in ["description_hash", "recurrence", "declined", "status"] {
            if changed(key) {
                message.push("\n");
                message.push(&tr.get(&format!("calendar-field-{key}")));
                message.push("\n");
            }
        }
    }
    if kind != "event.cancelled"
        && let Some(url) = calendar_link(event)
    {
        message.push("\n");
        message.styled(&tr.get("calendar-open"), EntityKind::TextLink { url });
    }
    message.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn long_unicode_changes_keep_valid_formatting_inside_the_message_limit() {
        let event = json!({"summary": "🚀".repeat(500), "location": "界".repeat(500),
            "organizer": {"displayName": "A".repeat(300)},
            "start": {"date": "2026-11-18"}, "end": {"date": "2026-11-19"}});
        let previous = json!({"summary": "🌙".repeat(500), "location": "語".repeat(500),
            "organizer": {"displayName": "B".repeat(300)},
            "start": {"date": "2026-11-17"}, "end": {"date": "2026-11-18"}});
        let rendered = notification(
            &Tr::new("en"),
            "event.changed",
            &json!({"event": event,
            "previous": previous, "changes": ["summary", "start", "location", "organizer"]}),
        );
        let utf16: Vec<u16> = rendered.text.encode_utf16().collect();
        assert!(utf16.len() <= 4096);
        assert!(rendered.text.ends_with('…'));
        assert!(!rendered.entities.is_empty());
        for entity in rendered.entities {
            assert!(entity.offset + entity.length <= utf16.len());
            assert!(
                String::from_utf16(&utf16[entity.offset..entity.offset + entity.length]).is_ok()
            );
        }
    }

    #[test]
    fn calendar_links_only_open_the_google_calendar_website() {
        for (url, linked) in [
            ("https://www.google.com/calendar/event?eid=abc", true),
            (
                "https://calendar.google.com/calendar/u/0/r/eventedit/abc",
                true,
            ),
            ("https://phish.example/calendar/event?eid=abc", false),
            ("javascript:alert(1)", false),
        ] {
            let rendered = notification(
                &Tr::new("en"),
                "event.changed",
                &json!({"event": {
                "summary": "<b>Literal title</b>", "htmlLink": url}, "previous": null}),
            );
            assert!(rendered.text.contains("<b>Literal title</b>"));
            assert_eq!(
                rendered
                    .entities
                    .iter()
                    .any(|e| matches!(e.kind, EntityKind::TextLink { .. })),
                linked
            );
        }
    }
}
