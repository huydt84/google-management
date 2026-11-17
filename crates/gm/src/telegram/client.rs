//! Telegram Bot API client: the [`Notifier`] for the owner's chat, plus the raw `call` the poller
//! uses for `getUpdates`. Plain text only (no `parse_mode`, invariant 9), link previews off. The
//! bot token is part of every URL, so URLs are never logged and are stripped from reqwest errors
//! (invariant 5).

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::notify::{Keyboard, Notifier, OutgoingMessage};

pub struct TelegramClient {
    http: reqwest::Client,
    api_base: String,
    token: String,
    chat_id: String,
}

fn keyboard_json(buttons: &Keyboard) -> Value {
    json!({"inline_keyboard": buttons.iter().map(|row| row.iter().map(|b| json!({"text": b.text, "callback_data": b.data})).collect::<Vec<_>>()).collect::<Vec<_>>()})
}

impl TelegramClient {
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

    /// One long poll: updates from `offset` on, waiting up to `timeout_s` seconds.
    pub async fn get_updates(&self, offset: i64, timeout_s: u64) -> anyhow::Result<Vec<Value>> {
        let r = self
            .call(
                "getUpdates",
                &json!({"offset": offset, "timeout": timeout_s,
                        "allowed_updates": ["callback_query", "message"]}),
            )
            .await?;
        Ok(r["result"].as_array().cloned().unwrap_or_default())
    }
}

#[async_trait]
impl Notifier for TelegramClient {
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
        if let Some(id) = msg.reply_to {
            // Still sent if the original was deleted meanwhile.
            body["reply_parameters"] =
                json!({"message_id": id, "allow_sending_without_reply": true});
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
