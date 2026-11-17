//! Telegram updates (the minimal part of workstream 3A, built in Phase 2 for the feedback buttons): a `getUpdates` long-poll loop
//! that only handles button presses (`callback_query`). No webhook, so gm stays bound to loopback
//! (invariant 6). Presses from any chat but `GM_TELEGRAM_CHAT_ID` are ignored. Each press is
//! answered right away (Telegram shows a spinner until then) and handed to the bus; the offset is
//! stored in `system_settings` after each update. Phase 3 extends this with messages and commands.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::bus::Bus;
use crate::feedback::{FeedbackReceived, parse_data};
use crate::notify::{Button, Keyboard, Notifier, TelegramNotifier};
use crate::topics;
use crate::user_settings::UserSettingsStore;

pub const OFFSET_KEY: &str = "telegram_offset";
/// Long-poll wait (seconds) per `getUpdates`.
pub const LONG_POLL_S: u64 = 25;

pub struct TelegramPoller {
    /// Bot API client with a timeout longer than the long poll.
    pub api: TelegramNotifier,
    pub chat_id: String,
    pub pool: PgPool,
    pub bus: Arc<Bus>,
    /// Answers presses (the app's notifier, so tests can record it).
    pub notifier: Arc<dyn Notifier>,
    pub user_settings: UserSettingsStore,
    pub owner: String,
}

fn keyboard_of(message: &Value) -> Keyboard {
    message["reply_markup"]["inline_keyboard"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|bs| {
                            bs.iter()
                                .map(|b| Button {
                                    text: b["text"].as_str().unwrap_or_default().into(),
                                    data: b["callback_data"].as_str().unwrap_or_default().into(),
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

impl TelegramPoller {
    /// Handle one update. Unknown update kinds and foreign chats are ignored.
    pub async fn handle_update(&self, update: &Value) -> anyhow::Result<()> {
        let cq = &update["callback_query"];
        let Some(cq_id) = cq["id"].as_str() else {
            return Ok(());
        };
        let chat = match &cq["message"]["chat"]["id"] {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => String::new(),
        };
        if chat != self.chat_id {
            tracing::warn!("button press from another chat ignored");
            self.notifier.answer_callback(cq_id, "").await?;
            return Ok(());
        }
        let tr = self.user_settings.tr(&self.owner).await;
        let data = cq["data"].as_str().unwrap_or_default();
        let Some((event_id, label)) = parse_data(data) else {
            self.notifier
                .answer_callback(cq_id, &tr.get("button-unknown"))
                .await?;
            return Ok(());
        };
        let payload = FeedbackReceived {
            event_id,
            label: label.into(),
            tapped_at: Utc::now(),
            message_id: cq["message"]["message_id"].as_i64(),
            keyboard: keyboard_of(&cq["message"]),
        };
        let mut tx = self.pool.begin().await?;
        self.bus
            .publish(
                &mut tx,
                topics::FEEDBACK_RECEIVED,
                &serde_json::to_value(&payload)?,
            )
            .await?;
        tx.commit().await?;
        self.notifier
            .answer_callback(cq_id, &tr.get("feedback-thanks"))
            .await?;
        Ok(())
    }

    /// One `getUpdates` round; returns how many updates were handled.
    pub async fn poll_once(&self) -> anyhow::Result<usize> {
        let offset = crate::db::system_get(&self.pool, OFFSET_KEY)
            .await?
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let r = self
            .api
            .call(
                "getUpdates",
                &json!({"offset": offset, "timeout": LONG_POLL_S, "allowed_updates": ["callback_query"]}),
            )
            .await?;
        let updates = r["result"].as_array().cloned().unwrap_or_default();
        for u in &updates {
            let Some(id) = u["update_id"].as_i64() else {
                continue;
            };
            if let Err(e) = self.handle_update(u).await {
                // Skipped rather than retried forever; the owner can tap again.
                tracing::error!(error = %e, update_id = id, "telegram update failed");
            }
            crate::db::system_set(&self.pool, OFFSET_KEY, &json!(id + 1)).await?;
        }
        Ok(updates.len())
    }

    pub async fn run(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(5);
        loop {
            match self.poll_once().await {
                Ok(_) => backoff = Duration::from_secs(5),
                Err(e) => {
                    tracing::warn!(error = %e, "telegram getUpdates failed");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(300));
                }
            }
        }
    }
}
