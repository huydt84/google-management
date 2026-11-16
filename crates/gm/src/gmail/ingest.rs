//! Gmail ingestion (ADR-0002): sync `history.list` from the stored cursor and turn added messages
//! into `gmail.message.received` events. Syncs are triggered by the poll loop (every
//! `GM_GMAIL_POLL_INTERVAL_S`, or every `GM_GMAIL_SAFETY_POLL_INTERVAL_S` while Pub/Sub works), by
//! Pub/Sub notifications ([`super::pubsub`]) and by the dashboard. The cursor, new `events` rows and bus deliveries are written in one transaction, so a
//! crash can never lose or double-publish a message. An expired cursor (HTTP 404) re-anchors and
//! recovers recent INBOX mail; event dedupe makes re-publishing harmless.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Mutex;

use crate::bus::Bus;
use crate::google::GoogleError;
use crate::google::gmail::{GmailClient, parse_i64};
use crate::topics;

const RECOVERY_QUERY: &str = "newer_than:2d";
const RECOVERY_MAX: u32 = 50;

pub struct GmailIngestor {
    pool: PgPool,
    bus: Arc<Bus>,
    gmail: GmailClient,
    labels: Vec<String>,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    pubsub_ok: AtomicBool,
    pubsub_error: std::sync::Mutex<Option<String>>,
}

impl GmailIngestor {
    pub fn new(pool: PgPool, bus: Arc<Bus>, gmail: GmailClient, labels: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            pool,
            bus,
            gmail,
            labels,
            locks: Mutex::new(HashMap::new()),
            pubsub_ok: AtomicBool::new(false),
            pubsub_error: std::sync::Mutex::new(None),
        })
    }

    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// Pub/Sub health, set by the Pub/Sub loop. While it is not OK the poll loop runs at the
    /// normal interval.
    pub fn pubsub_ok(&self) -> bool {
        self.pubsub_ok.load(Ordering::Relaxed)
    }

    pub fn pubsub_error(&self) -> Option<String> {
        self.pubsub_error.lock().unwrap().clone()
    }

    pub fn set_pubsub_ok(&self) {
        self.pubsub_ok.store(true, Ordering::Relaxed);
        *self.pubsub_error.lock().unwrap() = None;
    }

    pub fn set_pubsub_error(&self, err: &str) {
        self.pubsub_ok.store(false, Ordering::Relaxed);
        *self.pubsub_error.lock().unwrap() = Some(err.chars().take(500).collect());
    }

    fn wanted(&self, labels: &[String]) -> bool {
        labels.iter().any(|l| self.labels.contains(l))
    }

    /// Start the cursor at "now": only mail arriving after connecting is processed.
    pub async fn bootstrap(&self, email: &str) -> Result<(), GoogleError> {
        let hid = self.gmail.profile_history_id(email).await?;
        sqlx::query("INSERT INTO gmail_state (email, history_id) VALUES ($1, $2) ON CONFLICT (email) DO NOTHING")
            .bind(email)
            .bind(hid)
            .execute(&self.pool)
            .await
            .map_err(anyhow::Error::from)?;
        tracing::info!(account = email, history_id = hid, "gmail bootstrapped");
        Ok(())
    }

    pub(crate) async fn record_error(&self, email: &str, err: &str) {
        let err: String = err.chars().take(2000).collect();
        let _ = sqlx::query(
            "INSERT INTO gmail_state (email, last_error) VALUES ($1, $2)
             ON CONFLICT (email) DO UPDATE SET last_error = EXCLUDED.last_error, updated_at = now()",
        )
        .bind(email)
        .bind(err)
        .execute(&self.pool)
        .await;
    }

    /// Fetch changes since the stored cursor. Returns the number of new events.
    pub async fn sync(&self, email: &str) -> Result<usize, GoogleError> {
        let lock = self
            .locks
            .lock()
            .await
            .entry(email.to_string())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        match self.sync_locked(email).await {
            Ok(n) => Ok(n),
            Err(GoogleError::NotConnected(e)) => Err(GoogleError::NotConnected(e)),
            Err(e) => {
                self.record_error(email, &e.to_string()).await;
                Err(e)
            }
        }
    }

    async fn sync_locked(&self, email: &str) -> Result<usize, GoogleError> {
        let start: Option<i64> =
            sqlx::query_scalar("SELECT history_id FROM gmail_state WHERE email = $1")
                .bind(email)
                .fetch_optional(&self.pool)
                .await
                .map_err(anyhow::Error::from)?
                .flatten();
        let Some(start) = start else {
            self.bootstrap(email).await?;
            return Ok(0);
        };

        let mut found: Vec<(String, String)> = Vec::new(); // (message_id, thread_id), ordered, deduped
        let mut latest = start;
        let mut page: Option<String> = None;
        let outcome: Result<(), GoogleError> = async {
            loop {
                let resp = self
                    .gmail
                    .history_page(email, start, page.as_deref())
                    .await?;
                for record in resp["history"].as_array().into_iter().flatten() {
                    for added in record["messagesAdded"].as_array().into_iter().flatten() {
                        let m = &added["message"];
                        let labels: Vec<String> = m["labelIds"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|l| l.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let Some(id) = m["id"].as_str() else { continue };
                        if self.wanted(&labels) && !found.iter().any(|(f, _)| f == id) {
                            let thread = m["threadId"].as_str().unwrap_or(id).to_string();
                            found.push((id.to_string(), thread));
                        }
                    }
                }
                if let Some(h) = parse_i64(&resp["historyId"]) {
                    latest = latest.max(h);
                }
                page = resp["nextPageToken"].as_str().map(str::to_string);
                if page.is_none() {
                    return Ok(());
                }
            }
        }
        .await;
        match outcome {
            Ok(()) => {}
            Err(GoogleError::NotFound) => {
                tracing::warn!(
                    account = email,
                    start,
                    "gmail history expired; recovering recent messages"
                );
                (found, latest) = self.recover(email).await?;
            }
            Err(e) => return Err(e),
        }
        self.commit(email, &found, latest)
            .await
            .map_err(GoogleError::Other)
    }

    async fn recover(&self, email: &str) -> Result<(Vec<(String, String)>, i64), GoogleError> {
        let hid = self.gmail.profile_history_id(email).await?;
        let resp = self
            .gmail
            .list_messages(email, &self.labels, RECOVERY_QUERY, RECOVERY_MAX)
            .await?;
        let mut found: Vec<(String, String)> = resp["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let id = m["id"].as_str()?;
                Some((
                    id.to_string(),
                    m["threadId"].as_str().unwrap_or(id).to_string(),
                ))
            })
            .collect();
        found.reverse(); // oldest first
        Ok((found, hid))
    }

    async fn commit(
        &self,
        email: &str,
        found: &[(String, String)],
        latest: i64,
    ) -> anyhow::Result<usize> {
        let mut tx = self.pool.begin().await?;
        let mut published = 0;
        for (message_id, thread_id) in found {
            let id: Option<i64> = sqlx::query_scalar(
                "INSERT INTO events (account, source, kind, external_id, status, data)
                 VALUES ($1, 'gmail', 'message.received', $2, 'new', $3)
                 ON CONFLICT ON CONSTRAINT uq_events_source_ext DO NOTHING RETURNING id",
            )
            .bind(email)
            .bind(message_id)
            .bind(json!({ "thread_id": thread_id }))
            .fetch_optional(&mut *tx)
            .await?;
            let Some(event_id) = id else { continue }; // already seen
            let payload = json!({
                "event_id": event_id, "account": email, "message_id": message_id, "thread_id": thread_id
            });
            self.bus
                .publish(&mut tx, topics::GMAIL_MESSAGE_RECEIVED, &payload)
                .await?;
            published += 1;
        }
        sqlx::query(
            "INSERT INTO gmail_state (email, history_id, last_sync_at, last_error) VALUES ($1, $2, now(), NULL)
             ON CONFLICT (email) DO UPDATE SET history_id = GREATEST(COALESCE(gmail_state.history_id, 0), EXCLUDED.history_id),
                 last_sync_at = now(), last_error = NULL, updated_at = now()",
        )
        .bind(email)
        .bind(latest)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if published > 0 {
            tracing::info!(account = email, published, "gmail: new messages");
        }
        Ok(published)
    }

    pub async fn sync_all(&self) {
        let emails = match self.gmail.accounts().active_emails().await {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(error = %e, "listing accounts failed");
                return;
            }
        };
        for email in emails {
            match self.sync(&email).await {
                Ok(_) | Err(GoogleError::NotConnected(_)) => {}
                Err(e) => tracing::error!(account = %email, error = %e, "gmail sync failed"),
            }
        }
    }

    /// Poll every `interval`, or every `safety` while Pub/Sub is healthy (checked every 10 s, so a
    /// Pub/Sub failure brings the normal interval back quickly).
    pub async fn poll_loop(self: Arc<Self>, interval: Duration, safety: Option<Duration>) {
        tracing::info!(
            interval_s = interval.as_secs(),
            safety_s = safety.map(|s| s.as_secs()),
            "gmail poll loop"
        );
        loop {
            self.sync_all().await;
            let started = Instant::now();
            loop {
                let wait = match safety {
                    Some(s) if self.pubsub_ok() => s,
                    _ => interval,
                };
                let elapsed = started.elapsed();
                if elapsed >= wait {
                    break;
                }
                tokio::time::sleep((wait - elapsed).min(Duration::from_secs(10))).await;
            }
        }
    }
}
