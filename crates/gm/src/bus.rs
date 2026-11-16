//! Durable pub/sub on Postgres (ADR-0001).
//!
//! `publish` inserts one `jobs` row per subscriber of a topic **inside the caller's transaction**
//! (outbox pattern): if the business write commits, delivery is guaranteed; if it rolls back,
//! nothing is delivered. Workers claim rows with `FOR UPDATE SKIP LOCKED`. Delivery is
//! at-least-once, so handlers must be idempotent. Failures retry with exponential backoff
//! (5 s × 2^(n−1), capped at 900 s) and end in `dead` after `max_attempts`. A handler can also
//! `Defer` a job (for example until the model is back) without using up an attempt.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct JobContext {
    pub job_id: i64,
    pub topic: String,
    pub subscriber: String,
    pub attempt: i32,
    pub max_attempts: i32,
}

impl JobContext {
    pub fn is_last_attempt(&self) -> bool {
        self.attempt >= self.max_attempts
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done,
    /// Run again at this time; does not count as an attempt.
    Defer(DateTime<Utc>),
}

pub type HandlerFuture = Pin<Box<dyn Future<Output = anyhow::Result<Outcome>> + Send>>;
pub type Handler = Arc<dyn Fn(Value, JobContext) -> HandlerFuture + Send + Sync>;

struct Subscription {
    topic: String,
    handler: Handler,
    max_attempts: i32,
}

pub struct Bus {
    pool: PgPool,
    subs: RwLock<HashMap<String, Arc<Subscription>>>,
    wake: Notify,
    default_max_attempts: i32,
    idle_poll: Duration,
}

pub fn backoff_delay(attempt: i32) -> chrono::Duration {
    let secs = (5.0 * 2f64.powi(attempt.max(1) - 1)).min(900.0);
    chrono::Duration::milliseconds((secs * 1000.0) as i64)
}

impl Bus {
    pub fn new(pool: PgPool, default_max_attempts: i32) -> Self {
        Self {
            pool,
            subs: RwLock::new(HashMap::new()),
            wake: Notify::new(),
            default_max_attempts,
            idle_poll: Duration::from_secs(1),
        }
    }

    pub fn subscribe<F, Fut>(&self, topic: &str, name: &str, handler: F)
    where
        F: Fn(Value, JobContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<Outcome>> + Send + 'static,
    {
        let handler: Handler = Arc::new(move |p, c| Box::pin(handler(p, c)));
        let mut subs = self.subs.write().unwrap();
        assert!(
            !subs.contains_key(name),
            "subscriber {name:?} already registered"
        );
        subs.insert(
            name.to_string(),
            Arc::new(Subscription {
                topic: topic.to_string(),
                handler,
                max_attempts: self.default_max_attempts,
            }),
        );
    }

    fn subscribers_of(&self, topic: &str) -> Vec<(String, i32)> {
        let subs = self.subs.read().unwrap();
        let mut v: Vec<_> = subs
            .iter()
            .filter(|(_, s)| s.topic == topic)
            .map(|(n, s)| (n.clone(), s.max_attempts))
            .collect();
        v.sort();
        v
    }

    /// Stage deliveries on `conn` (normally an open transaction). Visible when the caller commits.
    pub async fn publish(
        &self,
        conn: &mut PgConnection,
        topic: &str,
        payload: &Value,
    ) -> anyhow::Result<usize> {
        let subs = self.subscribers_of(topic);
        if subs.is_empty() {
            tracing::warn!(topic, "publish with no subscribers; dropped");
        }
        for (name, max_attempts) in &subs {
            sqlx::query("INSERT INTO jobs (topic, subscriber, payload, max_attempts) VALUES ($1, $2, $3, $4)")
                .bind(topic)
                .bind(name)
                .bind(payload)
                .bind(max_attempts)
                .execute(&mut *conn)
                .await?;
        }
        self.wake.notify_waiters();
        Ok(subs.len())
    }

    pub async fn run_worker(self: Arc<Self>, worker_id: usize) {
        tracing::info!(worker_id, "bus worker started");
        loop {
            let processed = match self.run_once().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(worker_id, error = %e, "bus worker loop error");
                    false
                }
            };
            if !processed {
                let _ = tokio::time::timeout(self.idle_poll, self.wake.notified()).await;
            }
        }
    }

    /// Claim and process one job. Returns false when nothing was ready.
    pub async fn run_once(&self) -> anyhow::Result<bool> {
        let names: Vec<String> = self.subs.read().unwrap().keys().cloned().collect();
        if names.is_empty() {
            return Ok(false);
        }
        let row: Option<(i64, String, String, Value, i32, i32)> = sqlx::query_as(
            "UPDATE jobs SET status = 'running', attempts = attempts + 1, locked_at = now(), updated_at = now()
             WHERE id = (
                 SELECT id FROM jobs
                 WHERE status = 'pending' AND run_after <= now() AND subscriber = ANY($1)
                 ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED)
             RETURNING id, topic, subscriber, payload, attempts, max_attempts",
        )
        .bind(&names)
        .fetch_optional(&self.pool)
        .await?;
        let Some((job_id, topic, subscriber, payload, attempt, max_attempts)) = row else {
            return Ok(false);
        };
        let sub = self.subs.read().unwrap().get(&subscriber).cloned();
        let Some(sub) = sub else { return Ok(false) };
        let ctx = JobContext {
            job_id,
            topic,
            subscriber,
            attempt,
            max_attempts,
        };

        let result = (sub.handler)(payload, ctx.clone()).await;
        match result {
            Ok(Outcome::Done) => {
                sqlx::query("UPDATE jobs SET status = 'done', locked_at = NULL, updated_at = now() WHERE id = $1")
                    .bind(job_id)
                    .execute(&self.pool)
                    .await?;
            }
            Ok(Outcome::Defer(at)) => {
                tracing::info!(job_id, subscriber = %ctx.subscriber, %at, "job deferred");
                sqlx::query(
                    "UPDATE jobs SET status = 'pending', locked_at = NULL, attempts = attempts - 1,
                         run_after = $2, updated_at = now() WHERE id = $1",
                )
                .bind(job_id)
                .bind(at)
                .execute(&self.pool)
                .await?;
            }
            Err(e) => {
                let dead = ctx.is_last_attempt();
                let err = format!("{e:#}");
                let err: String = err
                    .chars()
                    .rev()
                    .take(4000)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                tracing::warn!(
                    job_id, topic = %ctx.topic, subscriber = %ctx.subscriber, attempt, max_attempts, dead,
                    error = %err, "job failed"
                );
                sqlx::query(
                    "UPDATE jobs SET status = $2, last_error = $3, locked_at = NULL, run_after = $4,
                         updated_at = now() WHERE id = $1",
                )
                .bind(job_id)
                .bind(if dead { "dead" } else { "pending" })
                .bind(err)
                .bind(Utc::now() + backoff_delay(attempt))
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(true)
    }

    /// Jobs stuck in `running` (process died mid-handler) go back to `pending`. At startup of the
    /// single-process deployment pass zero: anything still running belongs to a dead process.
    pub async fn requeue_stale(&self, older_than: chrono::Duration) -> anyhow::Result<u64> {
        let n = sqlx::query(
            "UPDATE jobs SET status = 'pending', locked_at = NULL, updated_at = now()
             WHERE status = 'running' AND (locked_at IS NULL OR locked_at < $1)",
        )
        .bind(Utc::now() - older_than)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n > 0 {
            tracing::warn!(n, "requeued stale running jobs");
        }
        Ok(n)
    }
}
