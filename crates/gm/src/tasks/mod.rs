//! Owner instructions and approvals (workstream 3B, ADR-0003). An instruction is a button tap or the
//! owner's words in reply to a notification. The **intent router** ([`router`]) reads only the
//! owner's words; each intent has a **recipe** in code ([`crate::recipes`]) where the model writes
//! content only. A recipe that wants an external effect returns [`RecipeResult::Propose`]: an
//! approval with an immutable, hashed payload that the owner approves on Telegram or the web, after
//! which an [`Executor`] re-validates and acts ([`approvals`]). Runtime: [`runtime`]; recipes and
//! executors by name: [`registry`].

pub mod approvals;
pub mod registry;
pub mod router;
pub mod runtime;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::db;
use crate::gmail::message::{ParsedMessage, parse_message};
use crate::google::GoogleError;
use crate::google::gmail::GmailClient;
use crate::llm::LlmClient;
use crate::user_settings::UserSettings;

/// What the owner asks for. `as_str` / `parse` are the only name mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Intent {
    Translate,
    Analyze,
    Reply,
    /// The router could not tell: the owner gets the usage hint.
    Unknown,
}

impl Intent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Intent::Translate => "translate",
            Intent::Analyze => "analyze",
            Intent::Reply => "reply",
            Intent::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "translate" => Some(Intent::Translate),
            "analyze" => Some(Intent::Analyze),
            "reply" => Some(Intent::Reply),
            "unknown" => Some(Intent::Unknown),
            _ => None,
        }
    }
}

/// `user.instruction`: a tap or the owner's words.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInstruction {
    /// The Telegram update behind it (`cb:<callback id>` or `msg:<message id>`); makes a
    /// redelivered job continue the same task instead of starting another.
    pub origin_key: String,
    #[serde(default)]
    pub event_id: Option<i64>,
    /// Continue this task (the owner replied to its question or to its approval preview).
    #[serde(default)]
    pub task_id: Option<i64>,
    /// The owner's words (trusted).
    #[serde(default)]
    pub text: Option<String>,
    /// Set by a button: no router call.
    #[serde(default)]
    pub intent: Option<Intent>,
    /// Telegram message the answers reply to.
    #[serde(default)]
    pub reply_to: Option<i64>,
}

/// `approval.decision`: Approve or Reject from Telegram or the web.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalDecision {
    pub approval_id: i64,
    pub approve: bool,
    /// `telegram` | `web`.
    pub via: String,
}

/// What a `task.reply` tells the owner. The text is rendered from the task and approval rows at
/// send time; the payload carries ids only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyKind {
    /// `tasks.result.text` (translation, analysis).
    Result,
    /// `tasks.result.question` (`needs_input`).
    Question,
    /// The approval's preview with Approve / Reject.
    Approval,
    /// The executor ran: `tasks.result.done`.
    Done,
    /// `tasks.error`.
    Failed,
    Rejected,
    /// The approval expired: its buttons are removed, nothing is sent.
    Expired,
    /// The router could not tell what the owner wants: usage hint.
    Unknown,
    /// The model is not answering; the task retries on its own.
    Waiting,
}

/// `task.reply`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskReply {
    pub task_id: i64,
    pub kind: ReplyKind,
    #[serde(default)]
    pub approval_id: Option<i64>,
    /// Random per reply; a redelivered job with a key already in `chat_messages` sends nothing.
    pub key: String,
}

impl TaskReply {
    /// The `task.reply` payload, with a fresh key.
    pub fn payload(task_id: i64, kind: ReplyKind, approval_id: Option<i64>) -> Value {
        json!(TaskReply {
            task_id,
            kind,
            approval_id,
            key: crate::google::oauth::random_token(16),
        })
    }
}

/// An error whose text is meant for the owner (already localized): the task fails at once with
/// it, without retries.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UserFacing(pub String);

impl UserFacing {
    pub fn of(e: &anyhow::Error) -> Option<&str> {
        e.downcast_ref::<UserFacing>().map(|u| u.0.as_str())
    }
}

pub struct RecipeContext<'a> {
    pub task_id: i64,
    /// The Google account the mail belongs to.
    pub account: &'a str,
    pub event: &'a db::Event,
    /// The owner's words, trusted; answers to questions are appended on new lines.
    pub instruction: Option<&'a str>,
    /// Router output (e.g. a translation target) and an edit request (`edit`).
    pub args: &'a Value,
    /// `tasks.result` so far: intermediate ids of an earlier attempt (e.g. the draft id).
    pub state: &'a Value,
    pub settings: &'a UserSettings,
    pub gmail: &'a GmailClient,
    pub llm: &'a LlmClient,
    pub pool: &'a PgPool,
}

impl RecipeContext<'_> {
    /// The mail again, fresh from Gmail (bodies are never stored). Deleted → [`UserFacing`].
    pub async fn message(&self) -> anyhow::Result<ParsedMessage> {
        match self
            .gmail
            .get_message(self.account, &self.event.external_id)
            .await
        {
            Ok(raw) => Ok(parse_message(&raw)),
            Err(GoogleError::NotFound) => Err(UserFacing(self.tr().get("task-mail-gone")).into()),
            Err(e) => Err(e.into()),
        }
    }

    /// Merge `patch` into `tasks.result` now, so a retried attempt finds it in `state`.
    pub async fn save(&self, patch: Value) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET result = result || $2, updated_at = now() WHERE id = $1")
            .bind(self.task_id)
            .bind(patch)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub fn tr(&self) -> crate::i18n::Tr {
        crate::i18n::Tr::new(&self.settings.ui_language)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecipeResult {
    /// Done: send this text.
    Text(String),
    /// Ask the owner; their reply continues the task.
    NeedsInput(String),
    /// An external effect: becomes an approval with this payload and preview.
    Propose {
        action: &'static str,
        payload: Value,
        preview: String,
    },
}

#[async_trait]
pub trait Recipe: Send + Sync {
    fn intent(&self) -> Intent;
    async fn run(&self, cx: RecipeContext<'_>) -> anyhow::Result<RecipeResult>;
}

pub struct ExecContext<'a> {
    pub task_id: i64,
    pub approval_id: i64,
    pub account: &'a str,
    pub settings: &'a UserSettings,
    pub gmail: &'a GmailClient,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecResult {
    /// The effect happened (or had already happened). `target` and `details` go to `audit_log`;
    /// `summary` is told to the owner.
    Done {
        target: String,
        details: Value,
        summary: String,
    },
    /// What would be executed no longer matches the approval (e.g. the draft was edited in
    /// Gmail): nothing happens and a new approval with this payload and preview is created.
    Changed { payload: Value, preview: String },
}

#[async_trait]
pub trait Executor: Send + Sync {
    fn action(&self) -> &'static str;
    async fn execute(&self, cx: ExecContext<'_>, payload: &Value) -> anyhow::Result<ExecResult>;
}

/// JSON with object keys sorted at every level, so equal payloads hash equally.
/// Written by hand so the order doesn't depend on serde_json's `preserve_order` feature.
pub fn canonical_json(v: &Value) -> String {
    fn write(v: &Value, out: &mut String) {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<_> = m.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String((*k).clone()).to_string());
                    out.push(':');
                    write(&m[*k], out);
                }
                out.push('}');
            }
            Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(x, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(v, &mut out);
    out
}

/// `sha256(canonical JSON)`, hex: stored with an approval and checked before execution.
pub fn payload_hash(payload: &Value) -> String {
    sha256_hex(canonical_json(payload).as_bytes())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_ignores_key_order_but_not_values() {
        let a = json!({"to": ["a@b.c"], "draft_id": "d1", "n": {"y": 1, "x": 2}});
        let b = json!({"n": {"x": 2, "y": 1}, "draft_id": "d1", "to": ["a@b.c"]});
        assert_eq!(payload_hash(&a), payload_hash(&b));
        let c = json!({"to": ["x@evil.example"], "draft_id": "d1", "n": {"y": 1, "x": 2}});
        assert_ne!(payload_hash(&a), payload_hash(&c));
        assert_eq!(
            canonical_json(&b),
            r#"{"draft_id":"d1","n":{"x":2,"y":1},"to":["a@b.c"]}"#
        );
    }

    #[test]
    fn intent_names_round_trip() {
        for i in [
            Intent::Translate,
            Intent::Analyze,
            Intent::Reply,
            Intent::Unknown,
        ] {
            assert_eq!(Intent::parse(i.as_str()), Some(i));
        }
        assert_eq!(Intent::parse("forward"), None);
    }
}
