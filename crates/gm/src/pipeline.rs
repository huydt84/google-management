//! Mail pipeline, wired as bus subscribers (docs/spec/events.md):
//!
//! `gmail.message.received` --triage--> `gmail.message.summarize` --summarize--> `notify.email` --notify-->
//! and for mail no rule covers: --triage--> `gmail.message.classify` --classify--> (as a rule would)
//!
//! Event states: `new → ignored | digest | classify | triaged → ready → notified`, `classify →
//! ignored | digest | triaged | ready`, or `gone`. Every handler
//! is idempotent: it checks the event's status before doing work, and a state change commits in
//! the same transaction as the next publish (invariant 1).

use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::bus::{Bus, JobContext, Outcome};
use crate::clock::{Clock, ModelHours};
use crate::gmail::message::{ParsedMessage, parse_message};
use crate::google::GoogleError;
use crate::google::accounts::AccountStore;
use crate::google::gmail::GmailClient;
use crate::i18n::Tr;
use crate::llm::{Lane, LlmClient, LlmUnavailable};
use crate::notify::{Notifier, OutgoingMessage, format_email_notification};
use crate::rules::{Action, Priority, RuleFile, Verdict};
use crate::topics;
use crate::user_settings::UserSettingsStore;

pub struct Pipeline {
    pub pool: PgPool,
    pub bus: Arc<Bus>,
    pub gmail: GmailClient,
    pub accounts: Arc<AccountStore>,
    pub rules: Arc<RuleFile>,
    pub llm: Arc<LlmClient>,
    pub notifier: Arc<dyn Notifier>,
    pub user_settings: UserSettingsStore,
    pub clock: Arc<dyn Clock>,
    pub hours: ModelHours,
    /// How long a summary waits before trying again while the model is unavailable.
    pub llm_unavailable_retry: chrono::Duration,
}

#[derive(Debug, serde::Deserialize)]
struct Received {
    event_id: i64,
    account: String,
    message_id: String,
}

#[derive(Debug, serde::Deserialize)]
struct EventRef {
    event_id: i64,
}

impl Pipeline {
    pub fn register(self: &Arc<Self>) {
        let p = self.clone();
        self.bus
            .subscribe(topics::GMAIL_MESSAGE_RECEIVED, "triage", move |v, c| {
                let p = p.clone();
                async move { p.triage(v, c).await }
            });
        let p = self.clone();
        self.bus
            .subscribe(topics::GMAIL_MESSAGE_CLASSIFY, "classify", move |v, c| {
                let p = p.clone();
                async move { p.classify(v, c).await }
            });
        let p = self.clone();
        self.bus
            .subscribe(topics::GMAIL_MESSAGE_SUMMARIZE, "summarize", move |v, c| {
                let p = p.clone();
                async move { p.summarize(v, c).await }
            });
        let p = self.clone();
        self.bus
            .subscribe(topics::NOTIFY_EMAIL, "notify", move |v, c| {
                let p = p.clone();
                async move { p.notify(v, c).await }
            });
    }

    async fn status(&self, event_id: i64) -> anyhow::Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT status FROM events WHERE id = $1")
                .bind(event_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub(crate) async fn set_status(
        &self,
        event_id: i64,
        expected: &str,
        new: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE events SET status = $3, updated_at = now() WHERE id = $1 AND status = $2",
        )
        .bind(event_id)
        .bind(expected)
        .bind(new)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch the event's mail. A mail deleted meanwhile moves the event from `status` to `gone` and
    /// returns `None`.
    pub(crate) async fn fetch_or_gone(
        &self,
        event_id: i64,
        status: &str,
        account: &str,
        message_id: &str,
    ) -> anyhow::Result<Option<ParsedMessage>> {
        match self.gmail.get_message(account, message_id).await {
            Ok(raw) => Ok(Some(parse_message(&raw))),
            Err(GoogleError::NotFound) => {
                self.set_status(event_id, status, "gone").await?;
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The steps every model job shares (decisions P4, L12): outside model hours the job waits for
    /// them; the mail is fetched (deleted → `gone`); `call` runs with the owner's summary language;
    /// while the model is unavailable the job waits `GM_LLM_UNAVAILABLE_RETRY_S` without using an
    /// attempt. `Err(outcome)` means the job stops here with that outcome.
    pub(crate) async fn model_step<T>(
        &self,
        event_id: i64,
        status: &str,
        account: &str,
        message_id: &str,
        task: &str,
        call: impl AsyncFnOnce(&ParsedMessage, &str) -> anyhow::Result<T>,
    ) -> anyhow::Result<Result<T, Outcome>> {
        let now = self.clock.now();
        if !self.hours.is_on(now) {
            return Ok(Err(Outcome::Defer(self.hours.next_on(now))));
        }
        let Some(msg) = self
            .fetch_or_gone(event_id, status, account, message_id)
            .await?
        else {
            return Ok(Err(Outcome::Done));
        };
        let owner = self.accounts.owner_of(account).await?;
        let prefs = self.user_settings.get(&owner).await?;
        match call(&msg, &prefs.summary_language).await {
            Ok(v) => Ok(Ok(v)),
            Err(e) if LlmUnavailable::is(&e) => {
                let retry_at = now.with_timezone(&chrono::Utc) + self.llm_unavailable_retry;
                tracing::warn!(event_id, task, error = %e, %retry_at, "model unavailable; job queued");
                Ok(Err(Outcome::Defer(retry_at)))
            }
            Err(e) => Err(e),
        }
    }

    pub async fn triage(&self, payload: Value, _ctx: JobContext) -> anyhow::Result<Outcome> {
        let p: Received = serde_json::from_value(payload.clone())?;
        if self.status(p.event_id).await?.as_deref() != Some("new") {
            return Ok(Outcome::Done);
        }
        let Some(msg) = self
            .fetch_or_gone(p.event_id, "new", &p.account, &p.message_id)
            .await?
        else {
            return Ok(Outcome::Done);
        };
        let verdict = self.rules.get().evaluate(&msg);
        let mut extra = msg.brief();
        extra["link"] = json!(msg.gmail_link(&p.account));
        self.apply_verdict(p.event_id, "new", &verdict, extra, &payload)
            .await?;
        Ok(Outcome::Done)
    }

    /// Where a verdict sends an event: its new status and the next topic, if any.
    pub fn route(verdict: &Verdict) -> (&'static str, Option<&'static str>) {
        match verdict.action {
            Action::Ignore => ("ignored", None),
            Action::Digest if verdict.summarize => ("digest", Some(topics::DIGEST_ITEM_SUMMARIZE)),
            Action::Digest => ("digest", None),
            Action::Triage => ("classify", Some(topics::GMAIL_MESSAGE_CLASSIFY)),
            Action::Notify if verdict.summarize => {
                ("triaged", Some(topics::GMAIL_MESSAGE_SUMMARIZE))
            }
            Action::Notify => ("ready", Some(topics::NOTIFY_EMAIL)),
        }
    }

    /// Record `verdict` (merged into `events.data` with `extra`) and publish the next stage in the
    /// same transaction, if the event is still in status `expected` (invariant 1).
    async fn apply_verdict(
        &self,
        event_id: i64,
        expected: &str,
        verdict: &Verdict,
        mut extra: Value,
        received: &Value,
    ) -> anyhow::Result<()> {
        let (status, next) = Self::route(verdict);
        extra["verdict"] = serde_json::to_value(verdict)?;
        let mut tx = self.pool.begin().await?;
        let current: Option<String> =
            sqlx::query_scalar("SELECT status FROM events WHERE id = $1 FOR UPDATE")
                .bind(event_id)
                .fetch_optional(&mut *tx)
                .await?;
        if current.as_deref() != Some(expected) {
            return Ok(());
        }
        sqlx::query("UPDATE events SET data = data || $2, rule = $3, status = $4, updated_at = now() WHERE id = $1")
            .bind(event_id)
            .bind(&extra)
            .bind(&verdict.rule)
            .bind(status)
            .execute(&mut *tx)
            .await?;
        if let Some(topic) = next {
            let next_payload = match topic {
                topics::GMAIL_MESSAGE_SUMMARIZE | topics::GMAIL_MESSAGE_CLASSIFY => {
                    received.clone()
                }
                _ => json!({"event_id": event_id}),
            };
            self.bus.publish(&mut tx, topic, &next_payload).await?;
        }
        tx.commit().await?;
        tracing::info!(event_id, rule = %verdict.rule, status, "event triaged");
        Ok(())
    }

    /// Model triage for mail no rule covers (2B). Like summaries it needs the model: outside model
    /// hours or while the model is unavailable the job waits (P4, L12).
    pub async fn classify(&self, payload: Value, _ctx: JobContext) -> anyhow::Result<Outcome> {
        let p: Received = serde_json::from_value(payload.clone())?;
        if self.status(p.event_id).await?.as_deref() != Some("classify") {
            return Ok(Outcome::Done);
        }
        let step = self
            .model_step(
                p.event_id,
                "classify",
                &p.account,
                &p.message_id,
                "triage",
                async |m, lang| crate::triage_model::classify(&self.llm, m, lang).await,
            )
            .await?;
        let decision = match step {
            Ok(d) => d,
            Err(outcome) => return Ok(outcome),
        };
        self.apply_verdict(
            p.event_id,
            "classify",
            &decision.verdict(),
            json!({}),
            &payload,
        )
        .await?;
        Ok(Outcome::Done)
    }

    pub fn verdict_of(data: &Value) -> Option<Verdict> {
        serde_json::from_value(data["verdict"].clone()).ok()
    }

    /// Summaries only run in model hours and only when the model answers: otherwise the job waits
    /// (without using an attempt) and no ping is sent without a summary (decisions P4, L12).
    /// Real errors retry with backoff and then leave the job `dead`.
    pub async fn summarize(&self, payload: Value, _ctx: JobContext) -> anyhow::Result<Outcome> {
        let p: Received = serde_json::from_value(payload)?;
        let Some(ev) = crate::db::event(&self.pool, p.event_id).await? else {
            return Ok(Outcome::Done);
        };
        if ev.status != "triaged" {
            return Ok(Outcome::Done);
        }
        let step = self
            .model_step(
                p.event_id,
                "triaged",
                &p.account,
                &p.message_id,
                "summary",
                async |m, lang| self.llm.summarize_email(m, lang, Lane::Notify).await,
            )
            .await?;
        let summary = match step {
            Ok(s) => serde_json::to_value(s)?,
            Err(outcome) => return Ok(outcome),
        };
        let mut tx = self.pool.begin().await?;
        let current: Option<String> =
            sqlx::query_scalar("SELECT status FROM events WHERE id = $1 FOR UPDATE")
                .bind(p.event_id)
                .fetch_optional(&mut *tx)
                .await?;
        if current.as_deref() != Some("triaged") {
            return Ok(Outcome::Done);
        }
        sqlx::query(
            "UPDATE events SET summary = $2, status = 'ready', updated_at = now() WHERE id = $1",
        )
        .bind(p.event_id)
        .bind(&summary)
        .execute(&mut *tx)
        .await?;
        self.bus
            .publish(
                &mut tx,
                topics::NOTIFY_EMAIL,
                &json!({"event_id": p.event_id}),
            )
            .await?;
        tx.commit().await?;
        Ok(Outcome::Done)
    }

    pub async fn notify(&self, payload: Value, _ctx: JobContext) -> anyhow::Result<Outcome> {
        let p: EventRef = serde_json::from_value(payload)?;
        let Some(ev) = crate::db::event(&self.pool, p.event_id).await? else {
            return Ok(Outcome::Done);
        };
        if ev.status != "ready" {
            return Ok(Outcome::Done);
        }
        let verdict = Self::verdict_of(&ev.data);
        let priority = verdict.as_ref().map(|v| v.priority).unwrap_or_default();
        let needs_llm = verdict.as_ref().is_none_or(|v| v.summarize);
        let now = self.clock.now();
        if needs_llm && !self.hours.is_on(now) {
            // Only LLM work waits for model hours (decision P4). Rule-only notifications (card
            // amount, login notice) go out at any hour; a summarized mail whose notification
            // slipped past 19:00 (e.g. Telegram retries) waits for 09:00.
            return Ok(Outcome::Defer(self.hours.next_on(now)));
        }
        let owner = self.accounts.owner_of(&ev.account).await?;
        let prefs = self.user_settings.get(&owner).await?;
        let tr = Tr::new(&prefs.ui_language);
        let text = format_email_notification(&tr, &ev.data, ev.summary.as_ref(), priority);
        self.notifier
            .send(&OutgoingMessage {
                text,
                silent: priority == Priority::Silent,
                buttons: vec![crate::notify::feedback_row(p.event_id, "", None)],
            })
            .await?;
        self.set_status(p.event_id, "ready", "notified").await?;
        Ok(Outcome::Done)
    }
}
