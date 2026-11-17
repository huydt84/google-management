//! Telegram updates (workstream 3A): a `getUpdates` long-poll loop, so gm needs no webhook and
//! stays bound to loopback (invariant 6). Only `GM_TELEGRAM_CHAT_ID` is served: updates from any
//! other chat are logged and ignored, never answered. The offset is stored in `system_settings`
//! after each update.
//!
//! - Button presses: `i:` → `user.instruction`, `a:` → `approval.decision` (the buttons are removed
//!   at once), `f:` → `feedback.received`. Each press is answered right away (Telegram shows a
//!   spinner until then).
//! - A reply to a bot message → `user.instruction` with the owner's words for the mail, task or
//!   approval that message belongs to (`chat_messages`). The incoming message is recorded in the
//!   same transaction as the publish, so a re-polled update is not handled twice.
//! - Commands: `/help`, `/status`, `/pause`, `/resume`, `/language [name]`.
//! - Anything else gets a short usage hint.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{Callback, ChatLog, MessageRefs, PAUSED_KEY, TelegramClient, parse_callback};
use crate::bus::Bus;
use crate::digest::ProblemSource;
use crate::feedback::FeedbackReceived;
use crate::i18n::Tr;
use crate::notify::{Button, Keyboard, Notifier, OutgoingMessage};
use crate::tasks::{ApprovalDecision, UserInstruction};
use crate::topics;
use crate::user_settings::{UserSettingsStore, normalize_language};

pub const OFFSET_KEY: &str = "telegram_offset";
/// Long-poll wait (seconds) per `getUpdates`.
pub const LONG_POLL_S: u64 = 25;

pub struct TelegramPoller {
    /// Bot API client with a timeout longer than the long poll.
    pub api: TelegramClient,
    pub chat_id: String,
    pub pool: PgPool,
    pub bus: Arc<Bus>,
    /// Answers presses and commands (the app's notifier, so tests can record it).
    pub notifier: Arc<dyn Notifier>,
    pub chat: ChatLog,
    pub user_settings: UserSettingsStore,
    pub owner: String,
    /// Current health problems for `/status`.
    pub problems: Option<Arc<dyn ProblemSource>>,
    pub model: std::sync::Arc<crate::model::ModelControl>,
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

fn chat_of(message: &Value) -> String {
    match &message["chat"]["id"] {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

impl TelegramPoller {
    /// Handle one update. Unknown update kinds and foreign chats are ignored.
    pub async fn handle_update(&self, update: &Value) -> anyhow::Result<()> {
        if update["callback_query"].is_object() {
            self.handle_callback(&update["callback_query"]).await
        } else if update["message"].is_object() {
            self.handle_message(&update["message"]).await
        } else {
            Ok(())
        }
    }

    async fn publish(&self, topic: &str, payload: &Value) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        self.bus.publish(&mut tx, topic, payload).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn handle_callback(&self, cq: &Value) -> anyhow::Result<()> {
        let Some(cq_id) = cq["id"].as_str() else {
            return Ok(());
        };
        if chat_of(&cq["message"]) != self.chat_id {
            tracing::warn!("button press from another chat ignored");
            self.notifier.answer_callback(cq_id, "").await?;
            return Ok(());
        }
        let tr = self.user_settings.tr(&self.owner).await;
        let message_id = cq["message"]["message_id"].as_i64();
        let data = cq["data"].as_str().unwrap_or_default();
        let toast = match parse_callback(data) {
            None => tr.get("button-unknown"),
            Some(Callback::Feedback { event_id, label }) => {
                let payload = FeedbackReceived {
                    event_id,
                    label: label.into(),
                    tapped_at: Utc::now(),
                    message_id,
                    keyboard: keyboard_of(&cq["message"]),
                };
                self.publish(topics::FEEDBACK_RECEIVED, &serde_json::to_value(&payload)?)
                    .await?;
                tr.get("feedback-thanks")
            }
            Some(Callback::Intent { event_id, intent }) => {
                let ins = UserInstruction {
                    origin_key: format!("cb:{cq_id}"),
                    event_id: Some(event_id),
                    task_id: None,
                    text: None,
                    intent: Some(intent),
                    reply_to: message_id,
                };
                self.publish(topics::USER_INSTRUCTION, &serde_json::to_value(&ins)?)
                    .await?;
                tr.get("task-working")
            }
            Some(Callback::Approval {
                approval_id,
                approve,
            }) => {
                let d = ApprovalDecision {
                    approval_id,
                    approve,
                    via: "telegram".into(),
                };
                self.publish(topics::APPROVAL_DECISION, &serde_json::to_value(&d)?)
                    .await?;
                if let Some(id) = message_id {
                    // No second tap: the outcome arrives as a reply.
                    self.notifier.edit_buttons(id, &vec![]).await?;
                }
                tr.get(if approve {
                    "approval-approving"
                } else {
                    "approval-rejecting"
                })
            }
        };
        self.notifier.answer_callback(cq_id, &toast).await?;
        Ok(())
    }

    async fn handle_message(&self, m: &Value) -> anyhow::Result<()> {
        if chat_of(m) != self.chat_id {
            tracing::warn!("message from another chat ignored");
            return Ok(());
        }
        let Some(message_id) = m["message_id"].as_i64() else {
            return Ok(());
        };
        let text = m["text"].as_str().unwrap_or_default().trim().to_string();
        let tr = self.user_settings.tr(&self.owner).await;
        let replied = m["reply_to_message"]["message_id"].as_i64();
        let refs = match replied {
            Some(id) => self.chat.lookup(id).await?.unwrap_or_default(),
            None => MessageRefs::default(),
        };
        let about_mail = refs.event_id.is_some() || refs.task_id.is_some();
        let mut tx = self.pool.begin().await?;
        if !self.chat.record_in(&mut tx, message_id, refs).await? {
            return Ok(()); // seen before: a re-polled update
        }
        if !text.starts_with('/') && about_mail && !text.is_empty() {
            let ins = UserInstruction {
                origin_key: format!("msg:{message_id}"),
                event_id: refs.event_id,
                task_id: refs.task_id,
                text: Some(text),
                intent: None,
                reply_to: Some(message_id),
            };
            self.bus
                .publish(
                    &mut tx,
                    topics::USER_INSTRUCTION,
                    &serde_json::to_value(&ins)?,
                )
                .await?;
            tx.commit().await?;
            return Ok(());
        }
        tx.commit().await?;
        let answer = if let Some(cmd) = text.strip_prefix('/') {
            self.command(cmd, &tr).await?
        } else if replied.is_some() && !about_mail {
            tr.get("chat-no-mail")
        } else {
            tr.get("chat-help")
        };
        self.notifier
            .send(&OutgoingMessage {
                text: answer,
                reply_to: Some(message_id),
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Run a command; returns the answer.
    async fn command(&self, cmd: &str, tr: &Tr) -> anyhow::Result<String> {
        let (name, arg) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
        // `/status@MyBot` in groups; harmless here.
        let name = name.split('@').next().unwrap_or_default().to_lowercase();
        let arg = arg.trim();
        Ok(match name.as_str() {
            "pause" => {
                crate::db::system_set(&self.pool, PAUSED_KEY, &json!(true)).await?;
                tracing::info!("notifications paused");
                tr.get("cmd-paused")
            }
            "resume" => {
                crate::db::system_set(&self.pool, PAUSED_KEY, &json!(false)).await?;
                let n = self.bus.run_now("notify").await?;
                tracing::info!(held = n, "notifications resumed");
                tr.args("cmd-resumed", &[("count", n.to_string())])
            }
            "status" => self.status_text(tr).await?,
            "language" => {
                let mut s = self.user_settings.get(&self.owner).await?;
                if arg.is_empty() {
                    tr.args("cmd-language-show", &[("language", s.summary_language)])
                } else {
                    match normalize_language(arg) {
                        Ok(l) => {
                            s.summary_language = l.clone();
                            match self.user_settings.save(&self.owner, s).await? {
                                Ok(_) => tr.args("cmd-language-set", &[("language", l)]),
                                Err(e) => tr.get(e.message_id()),
                            }
                        }
                        Err(e) => tr.get(e.message_id()),
                    }
                }
            }
            _ => tr.get("chat-help"),
        })
    }

    async fn status_text(&self, tr: &Tr) -> anyhow::Result<String> {
        let paused = super::paused(&self.pool).await?;
        let jobs = crate::db::count_by(&self.pool, "jobs").await?;
        let count = |s: &str| {
            jobs.iter()
                .find(|(k, _)| k == s)
                .map(|(_, n)| *n)
                .unwrap_or(0)
        };
        let hours = self.model.hours().label(&tr.get("always"));
        let problems = match &self.problems {
            Some(p) => p.problems(tr).await,
            None => vec![],
        };
        let mut lines = vec![
            tr.get("cmd-status-title"),
            tr.get(if paused {
                "cmd-status-paused"
            } else {
                "cmd-status-active"
            }),
            tr.args(
                "cmd-status-queue",
                &[
                    ("pending", count("pending").to_string()),
                    ("running", count("running").to_string()),
                    ("dead", count("dead").to_string()),
                ],
            ),
            format!("{}: {hours}", tr.get("model-hours")),
        ];
        if problems.is_empty() {
            lines.push(tr.get("cmd-status-ok"));
        } else {
            lines.push(tr.get("digest-system"));
            lines.extend(problems.into_iter().map(|p| format!("• {p}")));
        }
        Ok(lines.join("\n"))
    }

    /// One `getUpdates` round; returns how many updates were handled.
    pub async fn poll_once(&self) -> anyhow::Result<usize> {
        let offset = crate::db::system_get(&self.pool, OFFSET_KEY)
            .await?
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let updates = self.api.get_updates(offset, LONG_POLL_S).await?;
        for u in &updates {
            let Some(id) = u["update_id"].as_i64() else {
                continue;
            };
            if let Err(e) = self.handle_update(u).await {
                // Skipped rather than retried forever; the owner can tap or write again.
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
