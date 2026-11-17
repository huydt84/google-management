//! Reply: the model writes the body from the owner's words and the mail (thinking on); recipient,
//! subject and threading come from the original's headers ([`compose`]). The reply becomes a Gmail
//! draft and a proposed `gmail.send_draft`; nothing is sent until the owner approves, and
//! [`SendDraft`] re-checks the live draft first.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{owner_words, text_of, think_json};
use crate::gmail::message::parse_message;
use crate::google::GoogleError;
use crate::google::compose::{self, ReplyDraft, body_hash, decode_header};
use crate::i18n::Tr;
use crate::llm::strip_think;
use crate::tasks::{
    ExecContext, ExecResult, Executor, Intent, Recipe, RecipeContext, RecipeResult, UserFacing,
};

pub const REPLY_TASK: &str = include_str!("../../prompts/reply.md");
pub const MAX_OUTPUT_TOKENS: u32 = 1536;
/// The executor's action name.
pub const SEND_DRAFT: &str = "gmail.send_draft";

/// Google errors the owner can fix get a message for them.
fn google_error(e: GoogleError, tr: &Tr) -> anyhow::Error {
    match e {
        GoogleError::Http { status: 403, .. } => UserFacing(tr.get("reply-needs-scope")).into(),
        GoogleError::NeedsReconnect(_) | GoogleError::NotConnected(_) => {
            UserFacing(tr.get("reply-needs-scope")).into()
        }
        e => e.into(),
    }
}

/// What the owner sees before approving: recipient, subject and the full body.
pub fn preview(tr: &Tr, to: &[String], subject: &str, body: &str, changed: bool) -> String {
    let mut lines = Vec::new();
    if changed {
        lines.push(tr.get("reply-changed"));
        lines.push(String::new());
    }
    lines.push(tr.get("reply-preview-title"));
    lines.push(format!("{}: {}", tr.get("reply-to"), to.join(", ")));
    lines.push(format!("{}: {subject}", tr.get("reply-subject")));
    lines.push(String::new());
    lines.push(body.trim().to_string());
    lines.push(String::new());
    lines.push(tr.get("reply-preview-note"));
    lines.join("\n")
}

/// The approval payload: everything the executor needs and re-checks.
pub fn payload(account: &str, draft_id: &str, d: &ReplyDraft) -> Value {
    json!({
        "account": account,
        "draft_id": draft_id,
        "thread_id": d.thread_id,
        "to": d.to,
        "subject": d.subject,
        "body_sha256": body_hash(&d.body),
    })
}

pub struct Reply;

#[async_trait]
impl Recipe for Reply {
    fn intent(&self) -> Intent {
        Intent::Reply
    }

    async fn run(&self, cx: RecipeContext<'_>) -> anyhow::Result<RecipeResult> {
        let tr = cx.tr();
        let Some(instruction) = cx.instruction.map(str::trim).filter(|t| !t.is_empty()) else {
            return Ok(RecipeResult::NeedsInput(tr.get("reply-ask")));
        };
        let m = cx.message().await?;
        let headers = compose::reply_to(&m, "");
        if headers.to.is_empty() {
            return Err(UserFacing(tr.get("reply-no-recipient")).into());
        }
        let mut block = format!(
            "Owner's instruction (trusted):\n{}\n\n",
            owner_words(instruction)
        );
        if let (Some(edit), Some(current)) = (cx.args["edit"].as_str(), cx.state["body"].as_str()) {
            block.push_str(&format!(
                "Current draft:\n{}\n\nChange request (trusted):\n{}\n\n",
                owner_words(current),
                owner_words(edit)
            ));
        }
        block.push_str(&cx.llm.email_block(&m, usize::MAX));
        let reply = think_json(
            cx.llm,
            "reply",
            REPLY_TASK,
            "the language of the email, unless the instruction asks for another",
            &block,
            MAX_OUTPUT_TOKENS,
            |t| text_of(t, "body"),
        )
        .await?;
        let body = reply.parsed.unwrap_or_else(|| strip_think(&reply.raw));
        anyhow::ensure!(!body.trim().is_empty(), "empty reply body from the model");
        let draft = ReplyDraft {
            body: body.trim().to_string(),
            ..headers
        };
        let raw = draft.raw();
        let create = async || {
            cx.gmail
                .create_draft(cx.account, &raw, &draft.thread_id)
                .await
        };
        // A retried attempt or an edit updates the draft it already made.
        let draft_id = match cx.state["draft_id"].as_str() {
            Some(id) => match cx
                .gmail
                .update_draft(cx.account, id, &raw, &draft.thread_id)
                .await
            {
                Err(GoogleError::NotFound) => create().await,
                other => other,
            },
            None => create().await,
        }
        .map_err(|e| google_error(e, &tr))?;
        cx.save(json!({"draft_id": draft_id, "body": draft.body}))
            .await?;
        Ok(RecipeResult::Propose {
            action: SEND_DRAFT,
            payload: payload(cx.account, &draft_id, &draft),
            preview: preview(&tr, &draft.to, &draft.subject, &draft.body, false),
        })
    }
}

/// `gmail.send_draft {account, draft_id, thread_id, to, subject, body_sha256}`: send the draft only
/// if it still has exactly the approved recipients, subject, thread and body.
pub struct SendDraft;

fn strings(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[async_trait]
impl Executor for SendDraft {
    fn action(&self) -> &'static str {
        SEND_DRAFT
    }

    async fn execute(&self, cx: ExecContext<'_>, p: &Value) -> anyhow::Result<ExecResult> {
        let tr = Tr::new(&cx.settings.ui_language);
        let s = |k: &str| p[k].as_str().unwrap_or_default().to_string();
        let (draft_id, thread_id, subject, hash) = (
            s("draft_id"),
            s("thread_id"),
            s("subject"),
            s("body_sha256"),
        );
        let to = strings(&p["to"]);
        let done = |message_id: &str, note: &str| ExecResult::Done {
            target: to.join(", "),
            details: json!({"draft_id": draft_id, "thread_id": thread_id, "subject": subject,
                            "message_id": message_id, "note": note}),
            summary: tr.args(
                "reply-sent",
                &[("to", to.join(", ")), ("subject", subject.clone())],
            ),
        };
        match cx.gmail.get_draft(cx.account, &draft_id).await {
            Ok(d) => {
                let live = parse_message(&d["message"]);
                let mut live_to = live.to.clone();
                live_to.sort();
                let live_subject = decode_header(&live.subject);
                let live_hash = body_hash(&live.body_text);
                if live_to != to
                    || live_subject != subject
                    || live_hash != hash
                    || live.thread_id != thread_id
                {
                    let mut draft = ReplyDraft {
                        to: live.to.clone(),
                        subject: live_subject,
                        in_reply_to: String::new(),
                        references: String::new(),
                        thread_id: live.thread_id.clone(),
                        body: live.body_text.clone(),
                    };
                    draft.body = draft.body.trim().to_string();
                    return Ok(ExecResult::Changed {
                        payload: payload(cx.account, &draft_id, &draft),
                        preview: preview(&tr, &draft.to, &draft.subject, &draft.body, true),
                    });
                }
                let sent = cx
                    .gmail
                    .send_draft(cx.account, &draft_id)
                    .await
                    .map_err(|e| google_error(e, &tr))?;
                Ok(done(sent["id"].as_str().unwrap_or_default(), ""))
            }
            // Gone: sent already (a crash after drafts.send) if the thread has a SENT message
            // with this body; otherwise the owner deleted it.
            Err(GoogleError::NotFound) => {
                let thread = cx
                    .gmail
                    .get_thread(cx.account, &thread_id)
                    .await
                    .map_err(|e| google_error(e, &tr))?;
                let sent = thread["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|m| {
                        m["labelIds"]
                            .as_array()
                            .is_some_and(|l| l.iter().any(|x| x == "SENT"))
                            && body_hash(&parse_message(m).body_text) == hash
                    });
                match sent {
                    Some(m) => Ok(done(m["id"].as_str().unwrap_or_default(), "already sent")),
                    None => Err(UserFacing(tr.get("reply-draft-gone")).into()),
                }
            }
            Err(e) => Err(google_error(e, &tr)),
        }
    }
}
