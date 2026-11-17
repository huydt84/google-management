//! Approval rows: created from a recipe's proposal, decided once, expired by a sweep. The payload is
//! immutable; its hash is stored next to it and checked again right before the executor runs
//! (invariant 3).

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgConnection, PgPool};

use super::{ReplyKind, TaskReply, payload_hash};
use crate::bus::Bus;
use crate::topics;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Approval {
    pub id: i64,
    pub task_id: i64,
    pub action: String,
    pub payload: Value,
    pub payload_hash: String,
    pub preview: String,
    pub status: String,
    pub expires_at: DateTime<Utc>,
    pub decided_via: Option<String>,
    pub executing_job: Option<i64>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, task_id, action, payload, payload_hash, preview, status, expires_at, \
                       decided_via, executing_job, error, created_at";

impl Approval {
    /// Whether the stored payload still matches the hash taken when it was proposed.
    pub fn intact(&self) -> bool {
        payload_hash(&self.payload) == self.payload_hash
    }
}

/// Insert a pending approval on `conn`; returns its id.
pub async fn create(
    conn: &mut PgConnection,
    task_id: i64,
    action: &str,
    payload: &Value,
    preview: &str,
    expires_at: DateTime<Utc>,
) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "INSERT INTO approvals (task_id, action, payload, payload_hash, preview, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(task_id)
    .bind(action)
    .bind(payload)
    .bind(payload_hash(payload))
    .bind(preview)
    .bind(expires_at)
    .fetch_one(conn)
    .await
}

pub async fn get(pool: &PgPool, id: i64) -> sqlx::Result<Option<Approval>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM approvals WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Lock the row for a decision.
pub async fn lock(conn: &mut PgConnection, id: i64) -> sqlx::Result<Option<Approval>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM approvals WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await
}

pub async fn set_status(
    conn: &mut PgConnection,
    id: i64,
    status: &str,
    error: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE approvals SET status = $2, error = COALESCE($3, error), updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(error)
    .execute(conn)
    .await?;
    Ok(())
}

/// Pending approvals of the newest first, for the web page.
pub async fn pending(pool: &PgPool) -> sqlx::Result<Vec<Approval>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM approvals WHERE status = 'pending' AND expires_at > now() ORDER BY id DESC"
    ))
    .fetch_all(pool)
    .await
}

/// Expire pending approvals past `expires_at` (and their tasks), and tell the reply sender to take
/// their buttons away. One transaction. Returns how many expired.
pub async fn expire_due(pool: &PgPool, bus: &Bus) -> anyhow::Result<usize> {
    let mut tx = pool.begin().await?;
    let due: Vec<(i64, i64)> = sqlx::query_as(
        "UPDATE approvals SET status = 'expired', updated_at = now()
         WHERE status = 'pending' AND expires_at <= now() RETURNING id, task_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (approval_id, task_id) in &due {
        sqlx::query(
            "UPDATE tasks SET status = 'expired', updated_at = now()
             WHERE id = $1 AND status = 'awaiting_approval'",
        )
        .bind(task_id)
        .execute(&mut *tx)
        .await?;
        bus.publish(
            &mut tx,
            topics::TASK_REPLY,
            &TaskReply::payload(*task_id, ReplyKind::Expired, Some(*approval_id)),
        )
        .await?;
    }
    tx.commit().await?;
    if !due.is_empty() {
        tracing::info!(n = due.len(), "approvals expired");
    }
    Ok(due.len())
}

/// One row of `audit_log`, on `conn` (the transaction that records the outcome).
pub async fn audit(
    conn: &mut PgConnection,
    action: &str,
    target: &str,
    task_id: i64,
    details: &Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO audit_log (actor, action, target, task_id, details) VALUES ('owner', $1, $2, $3, $4)",
    )
    .bind(action)
    .bind(target)
    .bind(task_id)
    .bind(details)
    .execute(conn)
    .await?;
    Ok(())
}
