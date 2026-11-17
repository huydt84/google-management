//! Pub/Sub ingest mode (ADR-0002). gm provisions its topic and subscription, keeps a
//! Gmail watch on every active mailbox, pulls notifications and syncs the mailboxes they name.
//! Messages are acknowledged only after every sync succeeded, so a failure means redelivery, not
//! loss. The poll loop keeps running as a safety net (every 15 min while this loop is healthy,
//! every 60 s while it is not).

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::ingest::GmailIngestor;
use crate::google::gmail::GmailClient;
use crate::google::pubsub::PubSubClient;

/// Renew a watch when less than this remains (Gmail watches last 7 days).
const RENEW_BEFORE_DAYS: i64 = 6;
const MAX_BACKOFF: Duration = Duration::from_secs(300);

pub struct PubSubIngest {
    pool: PgPool,
    ingestor: Arc<GmailIngestor>,
    gmail: GmailClient,
    client: PubSubClient,
}

impl PubSubIngest {
    pub fn new(
        pool: PgPool,
        ingestor: Arc<GmailIngestor>,
        gmail: GmailClient,
        client: PubSubClient,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            ingestor,
            gmail,
            client,
        })
    }

    /// The connected account whose token carries the `pubsub` scope.
    async fn pubsub_account(&self) -> anyhow::Result<String> {
        let accounts = self.gmail.accounts();
        for email in accounts.active_emails().await? {
            if let Some(acc) = accounts.get(&email).await?
                && acc.scopes.0.iter().any(|s| s == crate::google::PUBSUB)
            {
                return Ok(email);
            }
        }
        bail!("no connected Google account has granted Pub/Sub access; reconnect Gmail")
    }

    /// Create the topic and subscription if needed and make sure every mailbox is watched.
    pub async fn prepare(&self) -> anyhow::Result<String> {
        let email = self.pubsub_account().await?;
        self.client.provision(&email).await?;
        self.ensure_watches().await?;
        Ok(email)
    }

    /// Start or renew the Gmail watch of each active mailbox whose watch is missing or expires in
    /// less than six days. Cheap when nothing is due (one query).
    pub async fn ensure_watches(&self) -> anyhow::Result<()> {
        let rows: Vec<(String, Option<DateTime<Utc>>)> = sqlx::query_as(
            "SELECT a.email, s.watch_expiration FROM google_accounts a
             LEFT JOIN gmail_state s ON s.email = a.email WHERE a.status = 'active' ORDER BY a.email",
        )
        .fetch_all(&self.pool)
        .await?;
        let due = Utc::now() + chrono::Duration::days(RENEW_BEFORE_DAYS);
        let topic = self.client.config().topic_name();
        for (email, expiration) in rows {
            if expiration.is_some_and(|e| e > due) {
                continue;
            }
            let expiration = match self
                .gmail
                .watch(&email, &topic, self.ingestor.labels())
                .await
            {
                Ok(e) => e,
                Err(e) => {
                    self.ingestor
                        .record_error(&email, &format!("Gmail watch failed: {e}"))
                        .await;
                    return Err(e.into());
                }
            };
            sqlx::query(
                "INSERT INTO gmail_state (email, watch_expiration) VALUES ($1, $2)
                 ON CONFLICT (email) DO UPDATE SET watch_expiration = EXCLUDED.watch_expiration, updated_at = now()",
            )
            .bind(&email)
            .bind(expiration)
            .execute(&self.pool)
            .await?;
            tracing::info!(account = %email, %expiration, "gmail watch renewed");
        }
        Ok(())
    }

    /// One pull: sync every mailbox named in the batch, then acknowledge it. Returns the number of
    /// messages acknowledged. A failed sync leaves the batch unacknowledged (redelivered after the
    /// 60 s ack deadline); unknown or malformed messages are acknowledged and dropped.
    pub async fn step(&self, email: &str) -> anyhow::Result<usize> {
        let pulled = self.client.pull(email, 20).await?;
        if pulled.is_empty() {
            return Ok(0);
        }
        let active: HashSet<String> = self
            .gmail
            .accounts()
            .active_emails()
            .await?
            .into_iter()
            .collect();
        let targets: BTreeSet<&String> = pulled
            .iter()
            .filter_map(|p| p.email.as_ref())
            .filter(|e| active.contains(*e))
            .collect();
        let mut all_ok = true;
        for account in targets {
            if let Err(e) = self.ingestor.sync(account).await {
                tracing::warn!(account = %account, error = %e, "sync after Pub/Sub notification failed; will be redelivered");
                all_ok = false;
            }
        }
        if !all_ok {
            return Ok(0);
        }
        let ids: Vec<String> = pulled.into_iter().map(|p| p.ack_id).collect();
        self.client.ack(email, &ids).await?;
        Ok(ids.len())
    }

    pub async fn run(self: Arc<Self>) {
        tracing::info!(
            subscription = %self.client.config().subscription_name(),
            "gmail pubsub loop"
        );
        let mut backoff = Duration::from_secs(5);
        loop {
            let result: anyhow::Result<()> = async {
                let email = self.prepare().await?;
                self.ingestor.set_pubsub_ok();
                backoff = Duration::from_secs(5);
                loop {
                    self.ensure_watches().await?;
                    self.step(&email).await?;
                    self.ingestor.set_pubsub_ok();
                }
            }
            .await;
            if let Err(e) = result {
                tracing::error!(error = %e, retry_s = backoff.as_secs(), "pubsub ingest failed; polling at the normal interval");
                self.ingestor.set_pubsub_error(&e.to_string());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}
