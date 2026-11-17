//! The task runtime, wired as bus subscribers (docs/spec/events.md):
//!
//! `user.instruction` --task_run--> router → recipe → `task.reply` (text, question or approval)
//! `approval.decision` --approval_decide--> lock, check status, expiry and hash → executor → audit →
//! `task.reply`
//! `task.reply` --task_reply--> Telegram, as a reply to the message the owner acted on
//!
//! Owner-initiated work runs whenever the model answers, also outside model hours (ADR-0015); while
//! the model is unavailable it waits `GM_LLM_UNAVAILABLE_RETRY_S` and the owner is told once.
//! Every state change commits in one transaction with the `task.reply` it causes (invariant 1).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::approvals::{self, Approval};
use super::registry::Registry;
use super::{
    ApprovalDecision, ExecContext, ExecResult, Intent, RecipeContext, RecipeResult, ReplyKind,
    TaskReply, UserFacing, UserInstruction, router,
};
use crate::bus::{Bus, JobContext, Outcome};
use crate::google::gmail::GmailClient;
use crate::i18n::Tr;
use crate::llm::{LlmClient, LlmUnavailable};
use crate::notify::{Keyboard, Notifier, OutgoingMessage, split_message};
use crate::telegram::{ChatLog, MessageRefs, approval_row};
use crate::topics;
use crate::user_settings::UserSettingsStore;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Task {
    pub id: i64,
    pub event_id: Option<i64>,
    pub origin: String,
    pub instruction: Option<String>,
    pub intent: Option<String>,
    pub args: Value,
    pub status: String,
    pub result: Value,
    pub error: Option<String>,
    pub reply_to: Option<i64>,
    pub created_at: DateTime<Utc>,
}

pub async fn task(pool: &PgPool, id: i64) -> sqlx::Result<Option<Task>> {
    sqlx::query_as(
        "SELECT id, event_id, origin, instruction, intent, args, status, result, error, reply_to, created_at
         FROM tasks WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub struct TaskRuntime {
    pub pool: PgPool,
    pub bus: Arc<Bus>,
    pub gmail: GmailClient,
    pub llm: Arc<LlmClient>,
    pub notifier: Arc<dyn Notifier>,
    pub chat: ChatLog,
    pub user_settings: UserSettingsStore,
    /// The gm user the Telegram chat belongs to (`GM_ALLOWED_EMAIL`).
    pub owner: String,
    pub registry: Registry,
    /// `GM_APPROVAL_TTL_H`.
    pub approval_ttl: chrono::Duration,
    pub llm_unavailable_retry: chrono::Duration,
}

fn joined(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

impl TaskRuntime {
    pub fn register(self: &Arc<Self>) {
        let r = self.clone();
        self.bus
            .subscribe(topics::USER_INSTRUCTION, "task_run", move |v, c| {
                let r = r.clone();
                async move { r.handle_instruction(v, c).await }
            });
        let r = self.clone();
        self.bus
            .subscribe(topics::APPROVAL_DECISION, "approval_decide", move |v, c| {
                let r = r.clone();
                async move { r.handle_decision(v, c).await }
            });
        let r = self.clone();
        self.bus
            .subscribe(topics::TASK_REPLY, "task_reply", move |v, c| {
                let r = r.clone();
                async move { r.handle_reply(v, c).await }
            });
    }

    async fn tr(&self) -> Tr {
        self.user_settings.tr(&self.owner).await
    }

    // ------------------------------------------------------------------ user.instruction

    pub async fn handle_instruction(
        &self,
        payload: Value,
        ctx: JobContext,
    ) -> anyhow::Result<Outcome> {
        let ins: UserInstruction = serde_json::from_value(payload)?;
        match self.accept(&ins).await? {
            Some(task_id) => self.run_task(task_id, &ctx).await,
            None => Ok(Outcome::Done),
        }
    }

    /// Create the task, or continue the one the owner answered. `None`: nothing to do.
    async fn accept(&self, ins: &UserInstruction) -> anyhow::Result<Option<i64>> {
        let text = ins.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
        let mut event_id = ins.event_id;
        if let Some(task_id) = ins.task_id {
            let mut tx = self.pool.begin().await?;
            let row: Option<(String, Option<i64>, Value)> = sqlx::query_as(
                "SELECT status, event_id, result FROM tasks WHERE id = $1 FOR UPDATE",
            )
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some((status, task_event, result)) = row {
                event_id = event_id.or(task_event);
                if result["last_origin"] == json!(ins.origin_key) {
                    return Ok(Some(task_id)); // a redelivered job: already applied
                }
                let origin = json!({"last_origin": ins.origin_key});
                match (status.as_str(), text) {
                    ("needs_input", Some(t)) => {
                        sqlx::query(
                            "UPDATE tasks SET status = 'pending', result = result || $2, reply_to = COALESCE($4, reply_to),
                                 instruction = CASE WHEN COALESCE(instruction, '') = '' THEN $3
                                                    ELSE instruction || E'\\n' || $3 END,
                                 updated_at = now() WHERE id = $1",
                        )
                        .bind(task_id)
                        .bind(&origin)
                        .bind(t)
                        .bind(ins.reply_to)
                        .execute(&mut *tx)
                        .await?;
                        tx.commit().await?;
                        return Ok(Some(task_id));
                    }
                    ("awaiting_approval", Some(t)) => {
                        // An edit request in reply to the preview: the old approval can no longer
                        // be approved; the recipe updates the draft and proposes again.
                        let old: Vec<i64> = sqlx::query_scalar(
                            "UPDATE approvals SET status = 'expired', updated_at = now()
                             WHERE task_id = $1 AND status = 'pending' RETURNING id",
                        )
                        .bind(task_id)
                        .fetch_all(&mut *tx)
                        .await?;
                        for id in old {
                            self.bus
                                .publish(
                                    &mut tx,
                                    topics::TASK_REPLY,
                                    &TaskReply::payload(task_id, ReplyKind::Expired, Some(id)),
                                )
                                .await?;
                        }
                        sqlx::query(
                            "UPDATE tasks SET status = 'pending', args = args || $2, result = result || $3,
                                 reply_to = COALESCE($4, reply_to), updated_at = now() WHERE id = $1",
                        )
                        .bind(task_id)
                        .bind(json!({"edit": t}))
                        .bind(&origin)
                        .bind(ins.reply_to)
                        .execute(&mut *tx)
                        .await?;
                        tx.commit().await?;
                        return Ok(Some(task_id));
                    }
                    // Finished tasks: the words start a new task on the same mail.
                    _ => {}
                }
            }
        }
        let Some(event_id) = event_id else {
            tracing::warn!(origin = %ins.origin_key, "instruction without a mail; ignored");
            return Ok(None);
        };
        if let Some(intent) = ins.intent {
            // A second tap on the same button: taps are queued, so the first may be done already.
            // Within two minutes it is the same request; after a failure the owner can retry.
            let busy: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM tasks WHERE event_id = $1 AND intent = $2
                     AND origin_key <> $3 AND status <> 'failed'
                     AND (status IN ('pending', 'running') OR created_at > now() - interval '2 minutes'))",
            )
            .bind(event_id)
            .bind(intent.as_str())
            .bind(&ins.origin_key)
            .fetch_one(&self.pool)
            .await?;
            if busy {
                tracing::info!(
                    event_id,
                    intent = intent.as_str(),
                    "same task already running"
                );
                return Ok(None);
            }
        }
        let inserted: Option<i64> = sqlx::query_scalar(
            "INSERT INTO tasks (origin_key, event_id, origin, instruction, intent, reply_to)
             VALUES ($1, $2, 'telegram', $3, $4, $5) ON CONFLICT (origin_key) DO NOTHING RETURNING id",
        )
        .bind(&ins.origin_key)
        .bind(event_id)
        .bind(text)
        .bind(ins.intent.map(|i| i.as_str()))
        .bind(ins.reply_to)
        .fetch_optional(&self.pool)
        .await?;
        match inserted {
            Some(id) => {
                tracing::info!(task_id = id, event_id, "task created");
                Ok(Some(id))
            }
            None => Ok(
                sqlx::query_scalar("SELECT id FROM tasks WHERE origin_key = $1")
                    .bind(&ins.origin_key)
                    .fetch_optional(&self.pool)
                    .await?,
            ),
        }
    }

    /// Run a pending (or interrupted) task: route, run its recipe, record the outcome.
    pub async fn run_task(&self, task_id: i64, ctx: &JobContext) -> anyhow::Result<Outcome> {
        let Some(mut t) = task(&self.pool, task_id).await? else {
            return Ok(Outcome::Done);
        };
        if !matches!(t.status.as_str(), "pending" | "running") {
            return Ok(Outcome::Done);
        }
        sqlx::query("UPDATE tasks SET status = 'running', updated_at = now() WHERE id = $1 AND status = 'pending'")
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        let settings = self.user_settings.get(&self.owner).await?;
        let tr = Tr::new(&settings.ui_language);

        let intent = match t.intent.as_deref().and_then(Intent::parse) {
            Some(i) => i,
            None => {
                let words = t.instruction.clone().unwrap_or_default();
                match router::route(&self.llm, &words).await {
                    Ok(r) => {
                        sqlx::query("UPDATE tasks SET intent = $2, args = args || $3, updated_at = now() WHERE id = $1")
                            .bind(task_id)
                            .bind(r.intent.as_str())
                            .bind(&r.args)
                            .execute(&self.pool)
                            .await?;
                        if let (Some(a), Some(b)) = (t.args.as_object_mut(), r.args.as_object()) {
                            a.extend(b.clone());
                        }
                        tracing::info!(task_id, intent = r.intent.as_str(), "task routed");
                        r.intent
                    }
                    Err(e) => return self.on_error(&t, e, ctx, &tr).await,
                }
            }
        };
        if intent == Intent::Unknown {
            self.finish(task_id, "done", json!({}), ReplyKind::Unknown)
                .await?;
            return Ok(Outcome::Done);
        }
        let recipe = self
            .registry
            .recipe(intent)
            .ok_or_else(|| anyhow::anyhow!("no recipe for {}", intent.as_str()))?;
        let event = match t.event_id {
            Some(id) => crate::db::event(&self.pool, id).await?,
            None => None,
        };
        let Some(event) = event else {
            let e = UserFacing(tr.get("task-mail-gone")).into();
            return self.on_error(&t, e, ctx, &tr).await;
        };
        let cx = RecipeContext {
            task_id,
            account: &event.account,
            event: &event,
            instruction: t.instruction.as_deref(),
            args: &t.args,
            state: &t.result,
            settings: &settings,
            gmail: &self.gmail,
            llm: &self.llm,
            pool: &self.pool,
        };
        match recipe.run(cx).await {
            Ok(RecipeResult::Text(text)) => {
                self.finish(task_id, "done", json!({"text": text}), ReplyKind::Result)
                    .await?
            }
            Ok(RecipeResult::NeedsInput(q)) => {
                self.finish(
                    task_id,
                    "needs_input",
                    json!({"question": q}),
                    ReplyKind::Question,
                )
                .await?
            }
            Ok(RecipeResult::Propose {
                action,
                payload,
                preview,
            }) => {
                let mut tx = self.pool.begin().await?;
                let moved = sqlx::query(
                    "UPDATE tasks SET status = 'awaiting_approval', updated_at = now() WHERE id = $1 AND status = 'running'",
                )
                .bind(task_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if moved == 1 {
                    let expires = Utc::now() + self.approval_ttl;
                    let id =
                        approvals::create(&mut tx, task_id, action, &payload, &preview, expires)
                            .await?;
                    self.bus
                        .publish(
                            &mut tx,
                            topics::TASK_REPLY,
                            &TaskReply::payload(task_id, ReplyKind::Approval, Some(id)),
                        )
                        .await?;
                    tracing::info!(task_id, approval_id = id, action, "approval proposed");
                }
                tx.commit().await?;
            }
            Err(e) => {
                t.status = "running".into();
                return self.on_error(&t, e, ctx, &tr).await;
            }
        }
        Ok(Outcome::Done)
    }

    /// Move a running task to `status`, merge `patch` into its result and queue the reply.
    async fn finish(
        &self,
        task_id: i64,
        status: &str,
        patch: Value,
        kind: ReplyKind,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(
            "UPDATE tasks SET status = $2, result = result || $3, updated_at = now() WHERE id = $1 AND status = 'running'",
        )
        .bind(task_id)
        .bind(status)
        .bind(patch)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 1 {
            self.bus
                .publish(
                    &mut tx,
                    topics::TASK_REPLY,
                    &TaskReply::payload(task_id, kind, None),
                )
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Model unavailable → wait (the owner is told once); a message for the owner, or the last
    /// attempt → `failed`; anything else → retry with backoff.
    async fn on_error(
        &self,
        t: &Task,
        e: anyhow::Error,
        ctx: &JobContext,
        tr: &Tr,
    ) -> anyhow::Result<Outcome> {
        if LlmUnavailable::is(&e) {
            let retry_at = Utc::now() + self.llm_unavailable_retry;
            tracing::warn!(task_id = t.id, error = %e, %retry_at, "model unavailable; task waits");
            if t.result["waiting_told"] != json!(true) {
                let mut tx = self.pool.begin().await?;
                sqlx::query("UPDATE tasks SET result = result || '{\"waiting_told\": true}', updated_at = now() WHERE id = $1")
                    .bind(t.id)
                    .execute(&mut *tx)
                    .await?;
                self.bus
                    .publish(
                        &mut tx,
                        topics::TASK_REPLY,
                        &TaskReply::payload(t.id, ReplyKind::Waiting, None),
                    )
                    .await?;
                tx.commit().await?;
            }
            return Ok(Outcome::Defer(retry_at));
        }
        let message = match UserFacing::of(&e) {
            Some(m) => m.to_string(),
            None if ctx.is_last_attempt() => tr.get("task-failed-generic"),
            None => return Err(e),
        };
        tracing::error!(task_id = t.id, error = %format!("{e:#}"), "task failed");
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(
            "UPDATE tasks SET status = 'failed', error = $2, updated_at = now() WHERE id = $1 AND status IN ('pending', 'running')",
        )
        .bind(t.id)
        .bind(&message)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 1 {
            self.bus
                .publish(
                    &mut tx,
                    topics::TASK_REPLY,
                    &TaskReply::payload(t.id, ReplyKind::Failed, None),
                )
                .await?;
        }
        tx.commit().await?;
        Ok(Outcome::Done)
    }

    // ------------------------------------------------------------------ approval.decision

    pub async fn handle_decision(
        &self,
        payload: Value,
        ctx: JobContext,
    ) -> anyhow::Result<Outcome> {
        let d: ApprovalDecision = serde_json::from_value(payload)?;
        let tr = self.tr().await;
        let mut tx = self.pool.begin().await?;
        let Some(a) = approvals::lock(&mut tx, d.approval_id).await? else {
            return Ok(Outcome::Done);
        };
        match a.status.as_str() {
            "pending" => {
                let (approval_status, task_status, kind, error) = if !d.approve {
                    ("rejected", "rejected", ReplyKind::Rejected, None)
                } else if a.expires_at <= Utc::now() {
                    ("expired", "expired", ReplyKind::Expired, None)
                } else if !a.intact() {
                    tracing::error!(
                        approval_id = a.id,
                        "approval payload does not match its hash; refused"
                    );
                    (
                        "failed",
                        "failed",
                        ReplyKind::Failed,
                        Some(tr.get("approval-tampered")),
                    )
                } else {
                    ("approved", "executing", ReplyKind::Done, None)
                };
                sqlx::query(
                    "UPDATE approvals SET status = $2, decided_at = now(), decided_via = $3,
                         executing_job = CASE WHEN $2 = 'approved' THEN $4 END, error = $5, updated_at = now()
                     WHERE id = $1",
                )
                .bind(a.id)
                .bind(approval_status)
                .bind(&d.via)
                .bind(ctx.job_id)
                .bind(&error)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE tasks SET status = $2, error = COALESCE($3, error), updated_at = now()
                     WHERE id = $1 AND status = 'awaiting_approval'",
                )
                .bind(a.task_id)
                .bind(task_status)
                .bind(&error)
                .execute(&mut *tx)
                .await?;
                if approval_status != "approved" {
                    tracing::info!(approval_id = a.id, status = approval_status, via = %d.via, "approval decided");
                    self.bus
                        .publish(
                            &mut tx,
                            topics::TASK_REPLY,
                            &TaskReply::payload(a.task_id, kind, Some(a.id)),
                        )
                        .await?;
                    tx.commit().await?;
                    return Ok(Outcome::Done);
                }
                tx.commit().await?;
                tracing::info!(approval_id = a.id, via = %d.via, "approval approved");
            }
            // This job approved it and stopped before recording the outcome: resume. The
            // executor is idempotent (it recognizes an already-sent draft).
            "approved" if a.executing_job == Some(ctx.job_id) => {
                tx.commit().await?;
                if !a.intact() {
                    anyhow::bail!("approval {} payload does not match its hash", a.id);
                }
            }
            // Decided already (a double tap, or Telegram and the web): nothing to do.
            other => {
                tracing::info!(
                    approval_id = a.id,
                    status = other,
                    "approval already decided"
                );
                return Ok(Outcome::Done);
            }
        }
        self.execute(&a, &d.via, &ctx, &tr).await
    }

    async fn execute(
        &self,
        a: &Approval,
        via: &str,
        ctx: &JobContext,
        tr: &Tr,
    ) -> anyhow::Result<Outcome> {
        let executor = self
            .registry
            .executor(&a.action)
            .ok_or_else(|| anyhow::anyhow!("no executor for {}", a.action))?;
        let settings = self.user_settings.get(&self.owner).await?;
        let account = a.payload["account"].as_str().unwrap_or_default();
        let cx = ExecContext {
            task_id: a.task_id,
            approval_id: a.id,
            account,
            settings: &settings,
            gmail: &self.gmail,
        };
        let outcome = executor.execute(cx, &a.payload).await;
        let mut tx = self.pool.begin().await?;
        match outcome {
            Ok(ExecResult::Done {
                target,
                mut details,
                summary,
            }) => {
                approvals::set_status(&mut tx, a.id, "executed", None).await?;
                sqlx::query(
                    "UPDATE tasks SET status = 'done', result = result || $2, updated_at = now() WHERE id = $1",
                )
                .bind(a.task_id)
                .bind(json!({"done": summary}))
                .execute(&mut *tx)
                .await?;
                details["approval_id"] = json!(a.id);
                details["via"] = json!(via);
                details["outcome"] = json!("executed");
                approvals::audit(&mut tx, &a.action, &target, a.task_id, &details).await?;
                self.bus
                    .publish(
                        &mut tx,
                        topics::TASK_REPLY,
                        &TaskReply::payload(a.task_id, ReplyKind::Done, Some(a.id)),
                    )
                    .await?;
                tracing::info!(approval_id = a.id, action = %a.action, "action executed");
            }
            Ok(ExecResult::Changed { payload, preview }) => {
                approvals::set_status(&mut tx, a.id, "failed", Some("changed before execution"))
                    .await?;
                sqlx::query(
                    "UPDATE tasks SET status = 'awaiting_approval', updated_at = now() WHERE id = $1",
                )
                .bind(a.task_id)
                .execute(&mut *tx)
                .await?;
                let expires = Utc::now() + self.approval_ttl;
                let id =
                    approvals::create(&mut tx, a.task_id, &a.action, &payload, &preview, expires)
                        .await?;
                self.bus
                    .publish(
                        &mut tx,
                        topics::TASK_REPLY,
                        &TaskReply::payload(a.task_id, ReplyKind::Approval, Some(id)),
                    )
                    .await?;
                tracing::warn!(
                    approval_id = a.id,
                    new_approval_id = id,
                    "changed before execution; new approval"
                );
            }
            Err(e) => {
                let message = match UserFacing::of(&e) {
                    Some(m) => m.to_string(),
                    None if ctx.is_last_attempt() => tr.get("task-failed-generic"),
                    None => return Err(e),
                };
                tracing::error!(approval_id = a.id, error = %format!("{e:#}"), "action failed");
                approvals::set_status(&mut tx, a.id, "failed", Some(&message)).await?;
                sqlx::query(
                    "UPDATE tasks SET status = 'failed', error = $2, updated_at = now() WHERE id = $1",
                )
                .bind(a.task_id)
                .bind(&message)
                .execute(&mut *tx)
                .await?;
                approvals::audit(
                    &mut tx,
                    &a.action,
                    &joined(&a.payload["to"]),
                    a.task_id,
                    &json!({"approval_id": a.id, "via": via, "outcome": "failed", "error": message}),
                )
                .await?;
                self.bus
                    .publish(
                        &mut tx,
                        topics::TASK_REPLY,
                        &TaskReply::payload(a.task_id, ReplyKind::Failed, Some(a.id)),
                    )
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(Outcome::Done)
    }

    // ------------------------------------------------------------------ task.reply

    pub async fn handle_reply(&self, payload: Value, _ctx: JobContext) -> anyhow::Result<Outcome> {
        let r: TaskReply = serde_json::from_value(payload)?;
        if self.chat.was_sent(&r.key).await? {
            return Ok(Outcome::Done);
        }
        let Some(t) = task(&self.pool, r.task_id).await? else {
            return Ok(Outcome::Done);
        };
        let tr = self.tr().await;
        let s = |k: &str| t.result[k].as_str().unwrap_or_default().to_string();
        // Approve / Reject is gone once the approval is decided, wherever that happened.
        if let Some(id) = r.approval_id
            && r.kind != ReplyKind::Approval
        {
            self.strip_buttons(id).await;
        }
        let (text, buttons): (String, Keyboard) = match r.kind {
            ReplyKind::Result => (s("text"), vec![]),
            ReplyKind::Question => (s("question"), vec![]),
            ReplyKind::Approval => {
                let Some(a) = approvals::get(&self.pool, r.approval_id.unwrap_or_default()).await?
                else {
                    return Ok(Outcome::Done);
                };
                if a.status != "pending" {
                    return Ok(Outcome::Done);
                }
                (a.preview, vec![approval_row(&tr, a.id)])
            }
            ReplyKind::Done => (s("done"), vec![]),
            ReplyKind::Failed => (
                tr.args(
                    "task-failed",
                    &[("error", t.error.clone().unwrap_or_default())],
                ),
                vec![],
            ),
            ReplyKind::Rejected => (tr.get("approval-rejected"), vec![]),
            ReplyKind::Expired => return Ok(Outcome::Done),
            ReplyKind::Unknown => (tr.get("chat-help-mail"), vec![]),
            ReplyKind::Waiting => (tr.get("task-waiting"), vec![]),
        };
        let refs = MessageRefs {
            event_id: t.event_id,
            task_id: Some(t.id),
            approval_id: r.approval_id,
        };
        send_parts(
            self.notifier.as_ref(),
            &self.chat,
            &text,
            buttons,
            t.reply_to,
            refs,
            Some(&r.key),
        )
        .await?;
        Ok(Outcome::Done)
    }

    async fn strip_buttons(&self, approval_id: i64) {
        let ids = match self.chat.approval_messages(approval_id).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "could not look up approval messages");
                return;
            }
        };
        for id in ids {
            if let Err(e) = self.notifier.edit_buttons(id, &vec![]).await {
                tracing::warn!(error = %e, message_id = id, "could not remove approval buttons");
            }
        }
    }
}

/// Send `text` in Telegram-sized parts: the first replies to `reply_to`, the last carries the
/// buttons; every part is recorded in `chat_messages` with `refs`.
pub async fn send_parts(
    notifier: &dyn Notifier,
    chat: &ChatLog,
    text: &str,
    buttons: Keyboard,
    reply_to: Option<i64>,
    refs: MessageRefs,
    reply_key: Option<&str>,
) -> anyhow::Result<()> {
    let parts = split_message(text);
    let last = parts.len() - 1;
    for (i, part) in parts.into_iter().enumerate() {
        let msg = OutgoingMessage {
            text: part,
            silent: false,
            buttons: if i == last { buttons.clone() } else { vec![] },
            reply_to: if i == 0 { reply_to } else { None },
        };
        if let Some(id) = notifier.send(&msg).await? {
            chat.record_out(id, refs, reply_key).await?;
        }
    }
    Ok(())
}
