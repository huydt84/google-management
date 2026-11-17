//! Scheduler (workstream 2A): publishes `schedule.due {kind, at, owner?}` at fixed local times.
//!
//! - `digest`: per user at their `digest_at` (decision N4).
//!
//! `schedule_runs` keeps the last occurrence fired per schedule: a time missed while the Mac slept
//! fires once on wake, never twice. A schedule seen for the first time is recorded without firing
//! (a fresh start at 08:00 doesn't send yesterday's digest). The record and the publish commit in
//! one transaction (invariant 1).

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::bus::Bus;
use crate::clock::{Clock, latest_occurrence};
use crate::topics;
use crate::user_settings::{UserSettingsStore, parse_digest_at};

pub struct Scheduler {
    pub pool: PgPool,
    pub bus: Arc<Bus>,
    pub clock: Arc<dyn Clock>,
    pub user_settings: UserSettingsStore,
    pub default_digest_at: NaiveTime,
}

impl Scheduler {
    /// Check every schedule once; returns how many fired.
    pub async fn tick(&self) -> anyhow::Result<usize> {
        let now = self.clock.now();
        let mut fired = 0;
        let owners: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT owner_email FROM google_accounts ORDER BY 1")
                .fetch_all(&self.pool)
                .await?;
        for owner in owners {
            let prefs = self.user_settings.get(&owner).await?;
            let t = parse_digest_at(&prefs.digest_at).unwrap_or(self.default_digest_at);
            let at = latest_occurrence(t, now);
            let payload = json!({"kind": "digest", "at": at.with_timezone(&Utc), "owner": owner});
            fired += usize::from(self.fire(&format!("digest:{owner}"), at, &payload).await?);
        }
        Ok(fired)
    }

    async fn fire(&self, name: &str, at: DateTime<Local>, payload: &Value) -> anyhow::Result<bool> {
        let at = at.with_timezone(&Utc);
        let mut tx = self.pool.begin().await?;
        let last: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT last_at FROM schedule_runs WHERE name = $1 FOR UPDATE")
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
        match last {
            None => {
                sqlx::query(
                    "INSERT INTO schedule_runs (name, last_at) VALUES ($1, $2) ON CONFLICT (name) DO NOTHING",
                )
                .bind(name)
                .bind(at)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(false)
            }
            Some(last) if last >= at => Ok(false),
            Some(_) => {
                sqlx::query(
                    "UPDATE schedule_runs SET last_at = $2, updated_at = now() WHERE name = $1",
                )
                .bind(name)
                .bind(at)
                .execute(&mut *tx)
                .await?;
                self.bus
                    .publish(&mut tx, topics::SCHEDULE_DUE, payload)
                    .await?;
                tx.commit().await?;
                tracing::info!(schedule = name, %at, "schedule due");
                Ok(true)
            }
        }
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            if let Err(e) = self.tick().await {
                tracing::error!(error = %e, "scheduler tick failed");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}
