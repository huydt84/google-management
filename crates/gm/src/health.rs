//! Health checks and alerts (workstream 2E, decisions O3, N12, N13).
//!
//! Every minute: dead jobs, ingest lag, model down during model hours, accounts needing a
//! reconnect. A check failing for 15 minutes sends one Telegram alert per incident (silent at
//! night), and a recovery message when it passes again. The dashboard panel and the digest's
//! "System" section show the same checks.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::clock::{Clock, ModelHours};
use crate::gmail::ingest::GmailIngestor;
use crate::i18n::Tr;
use crate::notify::{Notifier, OutgoingMessage};
use crate::user_settings::UserSettingsStore;

/// How long a check must fail before the alert (decision O3).
pub const ALERT_AFTER: chrono::Duration = chrono::Duration::minutes(15);
/// Ingest lag that counts as a failure (Pub/Sub mode adds the safety-poll interval, see `lag_limit`).
pub const INGEST_LAG: chrono::Duration = chrono::Duration::minutes(10);

pub const CHECKS: [&str; 4] = ["dead_jobs", "ingest_lag", "model_down", "needs_reconnect"];

/// Whether the model server answers its readiness probe (2xx within 5 s).
pub async fn model_ready(http: &reqwest::Client, ready_url: &str) -> bool {
    matches!(
        http.get(ready_url).timeout(Duration::from_secs(5)).send().await,
        Ok(r) if r.status().is_success()
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckResult {
    pub name: &'static str,
    /// `None`: not applicable now (e.g. the model check outside model hours); state is kept.
    pub failing: Option<bool>,
    /// Fluent arguments for `health-<name>`.
    pub args: Vec<(&'static str, String)>,
}

impl CheckResult {
    pub fn message(&self, tr: &Tr) -> String {
        tr.args(
            &format!("health-{}", self.name.replace('_', "-")),
            &self.args,
        )
    }
}

pub struct Health {
    pub pool: PgPool,
    pub http: reqwest::Client,
    pub ready_url: String,
    pub hours: ModelHours,
    pub clock: Arc<dyn Clock>,
    pub notifier: Arc<dyn Notifier>,
    pub user_settings: UserSettingsStore,
    pub ingestor: Arc<GmailIngestor>,
    /// `Some(safety poll seconds)` in Pub/Sub mode.
    pub pubsub_safety_poll_s: Option<u64>,
    /// Whose UI language alerts use (the single owner, `GM_ALLOWED_EMAIL`).
    pub owner: String,
}

impl Health {
    fn now(&self) -> DateTime<Utc> {
        self.clock.now().with_timezone(&Utc)
    }

    /// While Pub/Sub works, a quiet mailbox is only polled every safety interval (15 min), so the
    /// limit is that interval + 10 min; otherwise 10 min.
    fn lag_limit(&self) -> chrono::Duration {
        match self.pubsub_safety_poll_s {
            Some(s) if self.ingestor.pubsub_ok() => {
                chrono::Duration::seconds(s as i64) + INGEST_LAG
            }
            _ => INGEST_LAG,
        }
    }

    /// Run every check once.
    pub async fn evaluate(&self) -> anyhow::Result<Vec<CheckResult>> {
        let now = self.now();
        let dead: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE status = 'dead'")
            .fetch_one(&self.pool)
            .await?;
        let lagging: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT min(COALESCE(s.last_sync_at, a.created_at)) FROM google_accounts a
             LEFT JOIN gmail_state s ON s.email = a.email WHERE a.status = 'active'",
        )
        .fetch_one(&self.pool)
        .await?;
        let reconnect: Vec<String> = sqlx::query_scalar(
            "SELECT email FROM google_accounts WHERE status = 'needs_reconnect' ORDER BY email",
        )
        .fetch_all(&self.pool)
        .await?;
        let lag = lagging.map(|t| now - t);
        let model = if self.hours.is_on(self.clock.now()) {
            Some(!model_ready(&self.http, &self.ready_url).await)
        } else {
            None
        };
        Ok(vec![
            CheckResult {
                name: "dead_jobs",
                failing: Some(dead > 0),
                args: vec![("count", dead.to_string())],
            },
            CheckResult {
                name: "ingest_lag",
                failing: lag.map(|l| l > self.lag_limit()),
                args: vec![(
                    "minutes",
                    lag.map(|l| l.num_minutes()).unwrap_or(0).to_string(),
                )],
            },
            CheckResult {
                name: "model_down",
                failing: model,
                args: vec![],
            },
            CheckResult {
                name: "needs_reconnect",
                failing: Some(!reconnect.is_empty()),
                args: vec![("account", reconnect.join(", "))],
            },
        ])
    }

    async fn tr(&self) -> Tr {
        match self.user_settings.get(&self.owner).await {
            Ok(p) => Tr::new(&p.ui_language),
            Err(_) => Tr::new(&self.user_settings.defaults().ui_language),
        }
    }

    async fn send(&self, text: String) -> bool {
        // Silent at night (decision N12).
        let silent = !self.hours.is_on(self.clock.now());
        match self
            .notifier
            .send(&OutgoingMessage::text(text, silent))
            .await
        {
            Ok(_) => true,
            Err(e) => {
                tracing::error!(error = %e, "health alert could not be sent; retrying next minute");
                false
            }
        }
    }

    /// One round: update incidents, alert after 15 minutes, announce recoveries.
    pub async fn tick(&self) -> anyhow::Result<()> {
        let now = self.now();
        let tr = self.tr().await;
        for r in self.evaluate().await? {
            match r.failing {
                None => {}
                Some(true) => {
                    let msg = r.message(&tr);
                    let (since, alerted): (DateTime<Utc>, Option<DateTime<Utc>>) = sqlx::query_as(
                        "INSERT INTO health_state (check_name, failing_since, detail) VALUES ($1, $2, $3)
                         ON CONFLICT (check_name) DO UPDATE SET detail = EXCLUDED.detail, updated_at = now()
                         RETURNING failing_since, alerted_at",
                    )
                    .bind(r.name)
                    .bind(now)
                    .bind(&msg)
                    .fetch_one(&self.pool)
                    .await?;
                    if alerted.is_none() && now - since >= ALERT_AFTER {
                        let minutes = (now - since).num_minutes().to_string();
                        let text = tr.args(
                            "health-alert",
                            &[("minutes", minutes), ("problem", msg.clone())],
                        );
                        tracing::warn!(check = r.name, problem = %msg, "health alert");
                        if self.send(text).await {
                            sqlx::query(
                                "UPDATE health_state SET alerted_at = $2 WHERE check_name = $1",
                            )
                            .bind(r.name)
                            .bind(now)
                            .execute(&self.pool)
                            .await?;
                        }
                    }
                }
                Some(false) => {
                    let gone: Option<(Option<DateTime<Utc>>, String)> = sqlx::query_as(
                        "DELETE FROM health_state WHERE check_name = $1 RETURNING alerted_at, detail",
                    )
                    .bind(r.name)
                    .fetch_optional(&self.pool)
                    .await?;
                    if let Some((Some(_), detail)) = gone {
                        tracing::info!(check = r.name, "health recovered");
                        self.send(tr.args("health-recovered", &[("problem", detail)]))
                            .await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Open incidents: check name → failing since.
    pub async fn incidents(&self) -> anyhow::Result<Vec<(String, DateTime<Utc>)>> {
        Ok(
            sqlx::query_as("SELECT check_name, failing_since FROM health_state ORDER BY 1")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            if let Err(e) = self.tick().await {
                tracing::error!(error = %e, "health check failed");
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

#[async_trait]
impl crate::digest::ProblemSource for Health {
    async fn problems(&self, tr: &Tr) -> Vec<String> {
        match self.evaluate().await {
            Ok(results) => results
                .iter()
                .filter(|r| r.failing == Some(true))
                .map(|r| r.message(tr))
                .collect(),
            Err(e) => {
                tracing::error!(error = %e, "health check for the digest failed");
                vec![]
            }
        }
    }
}
