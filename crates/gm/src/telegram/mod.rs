//! Telegram, the mobile channel (ADR-0006): the Bot API client ([`client`]), the `getUpdates`
//! poller that turns taps, replies and commands into bus messages ([`poller`]), callback data, and
//! the map from Telegram messages to the mail, task or approval they belong to (`chat_messages`).

pub mod client;
pub mod poller;

use std::sync::LazyLock;

use regex::Regex;
use sqlx::{PgConnection, PgPool};

pub use client::TelegramClient;
pub use poller::TelegramPoller;

use crate::i18n::Tr;
use crate::notify::Button;
use crate::tasks::Intent;

/// `system_settings` key: `true` while `/pause` holds mail notifications.
pub const PAUSED_KEY: &str = "notifications_paused";

/// What a button press asks for. Callback data carries ids only (≤ 64 bytes) and is parsed
/// strictly; anything else is "no longer supported".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    /// `i:<event_id>:<intent>`: Translate / Analyze / Reply under a notification.
    Intent { event_id: i64, intent: Intent },
    /// `a:<approval_id>:y|n`: Approve / Reject under a proposed action.
    Approval { approval_id: i64, approve: bool },
    /// `f:<event_id>:up|down`: triage feedback ([`crate::feedback`]).
    Feedback { event_id: i64, label: &'static str },
}

static INTENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^i:(\d{1,18}):(translate|analyze|reply)$").unwrap());
static APPROVAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^a:(\d{1,18}):(y|n)$").unwrap());

pub fn parse_callback(data: &str) -> Option<Callback> {
    if let Some(c) = INTENT_RE.captures(data) {
        return Some(Callback::Intent {
            event_id: c[1].parse().ok()?,
            intent: Intent::parse(&c[2])?,
        });
    }
    if let Some(c) = APPROVAL_RE.captures(data) {
        return Some(Callback::Approval {
            approval_id: c[1].parse().ok()?,
            approve: &c[2] == "y",
        });
    }
    let (event_id, label) = crate::feedback::parse_data(data)?;
    Some(Callback::Feedback { event_id, label })
}

/// `[Translate] [Analyze] [Reply]` under a mail notification.
pub fn intent_row(tr: &Tr, event_id: i64) -> Vec<Button> {
    [Intent::Translate, Intent::Analyze, Intent::Reply]
        .into_iter()
        .map(|i| Button {
            text: tr.get(&format!("button-{}", i.as_str())),
            data: format!("i:{event_id}:{}", i.as_str()),
        })
        .collect()
}

/// `[Approve] [Reject]` under a proposed action.
pub fn approval_row(tr: &Tr, approval_id: i64) -> Vec<Button> {
    vec![
        Button {
            text: tr.get("button-approve"),
            data: format!("a:{approval_id}:y"),
        },
        Button {
            text: tr.get("button-reject"),
            data: format!("a:{approval_id}:n"),
        },
    ]
}

/// What a Telegram message refers to.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct MessageRefs {
    pub event_id: Option<i64>,
    pub task_id: Option<i64>,
    pub approval_id: Option<i64>,
}

/// The `chat_messages` map for the one configured chat.
#[derive(Clone)]
pub struct ChatLog {
    pool: PgPool,
    chat_id: String,
}

impl ChatLog {
    pub fn new(pool: PgPool, chat_id: &str) -> Self {
        Self {
            pool,
            chat_id: chat_id.to_string(),
        }
    }

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    /// Remember a message the bot sent. A message id seen before keeps its first row.
    pub async fn record_out(
        &self,
        message_id: i64,
        refs: MessageRefs,
        reply_key: Option<&str>,
    ) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO chat_messages (chat_id, message_id, event_id, task_id, approval_id, direction, reply_key)
             VALUES ($1, $2, $3, $4, $5, 'out', $6) ON CONFLICT (chat_id, message_id) DO NOTHING",
        )
        .bind(&self.chat_id)
        .bind(message_id)
        .bind(refs.event_id)
        .bind(refs.task_id)
        .bind(refs.approval_id)
        .bind(reply_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record an incoming message on `conn` (the transaction that also publishes what it asks
    /// for). Returns false if it was already recorded: a re-polled update.
    pub async fn record_in(
        &self,
        conn: &mut PgConnection,
        message_id: i64,
        refs: MessageRefs,
    ) -> sqlx::Result<bool> {
        let r = sqlx::query(
            "INSERT INTO chat_messages (chat_id, message_id, event_id, task_id, approval_id, direction)
             VALUES ($1, $2, $3, $4, $5, 'in') ON CONFLICT (chat_id, message_id) DO NOTHING",
        )
        .bind(&self.chat_id)
        .bind(message_id)
        .bind(refs.event_id)
        .bind(refs.task_id)
        .bind(refs.approval_id)
        .execute(conn)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    pub async fn lookup(&self, message_id: i64) -> sqlx::Result<Option<MessageRefs>> {
        sqlx::query_as(
            "SELECT event_id, task_id, approval_id FROM chat_messages WHERE chat_id = $1 AND message_id = $2",
        )
        .bind(&self.chat_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Whether the `task.reply` with this key was already sent.
    pub async fn was_sent(&self, reply_key: &str) -> sqlx::Result<bool> {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM chat_messages WHERE reply_key = $1)")
            .bind(reply_key)
            .fetch_one(&self.pool)
            .await
    }

    /// Messages that show an approval's buttons.
    pub async fn approval_messages(&self, approval_id: i64) -> sqlx::Result<Vec<i64>> {
        sqlx::query_scalar(
            "SELECT message_id FROM chat_messages WHERE chat_id = $1 AND approval_id = $2 AND direction = 'out'",
        )
        .bind(&self.chat_id)
        .bind(approval_id)
        .fetch_all(&self.pool)
        .await
    }
}

/// Whether `/pause` is on.
pub async fn paused(pool: &PgPool) -> sqlx::Result<bool> {
    Ok(crate::db::system_get(pool, PAUSED_KEY)
        .await?
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callbacks_are_parsed_strictly() {
        assert_eq!(
            parse_callback("i:12:translate"),
            Some(Callback::Intent {
                event_id: 12,
                intent: Intent::Translate
            })
        );
        assert_eq!(
            parse_callback("a:7:y"),
            Some(Callback::Approval {
                approval_id: 7,
                approve: true
            })
        );
        assert_eq!(
            parse_callback("a:7:n"),
            Some(Callback::Approval {
                approval_id: 7,
                approve: false
            })
        );
        assert_eq!(
            parse_callback("f:3:up"),
            Some(Callback::Feedback {
                event_id: 3,
                label: "up"
            })
        );
        for bad in [
            "i:12:forward",
            "i:12:unknown",
            "a:7:yes",
            "a:x:y",
            "i:12:reply:x",
            "a:1234567890123456789:y",
            "",
        ] {
            assert_eq!(parse_callback(bad), None, "{bad}");
        }
    }

    #[test]
    fn button_data_fits_telegram() {
        let tr = Tr::new("ja");
        let max_id = 999_999_999_999_999_999; // 18 digits, the most the parser accepts
        for b in intent_row(&tr, max_id)
            .into_iter()
            .chain(approval_row(&tr, max_id))
        {
            assert!(b.data.len() <= 64, "{}", b.data);
            assert!(parse_callback(&b.data).is_some(), "{}", b.data);
        }
    }
}
