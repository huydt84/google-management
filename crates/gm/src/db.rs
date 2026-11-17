//! Postgres pool, migrations and row types. Schema: `migrations/`, spec: docs/spec/data-model.md.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(10)
        .connect(url)
        .await?)
}

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    MIGRATOR.run(pool).await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct GoogleAccount {
    pub email: String,
    pub owner_email: String,
    pub refresh_token_enc: String,
    pub scopes: sqlx::types::Json<Vec<String>>,
    pub status: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct GmailState {
    pub email: String,
    pub history_id: Option<i64>,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct Event {
    pub id: i64,
    pub account: String,
    pub source: String,
    pub kind: String,
    pub external_id: String,
    pub status: String,
    pub rule: Option<String>,
    pub data: Value,
    pub summary: Option<Value>,
    pub created_at: DateTime<Utc>,
}

pub async fn event(pool: &PgPool, id: i64) -> sqlx::Result<Option<Event>> {
    sqlx::query_as::<_, Event>(
        "SELECT id, account, source, kind, external_id, status, rule, data, summary, created_at
         FROM events WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn recent_events(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<Event>> {
    sqlx::query_as::<_, Event>(
        "SELECT id, account, source, kind, external_id, status, rule, data, summary, created_at
         FROM events ORDER BY id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn count_by(pool: &PgPool, table: &str) -> sqlx::Result<Vec<(String, i64)>> {
    // `table` is one of our own constants, never user input.
    debug_assert!(matches!(table, "events" | "jobs"));
    sqlx::query_as::<_, (String, i64)>(&format!(
        "SELECT status, count(*) FROM {table} GROUP BY status ORDER BY status"
    ))
    .fetch_all(pool)
    .await
}

/// Delete events, digests, tasks (with their approvals), chat message records and LLM call records
/// older than the retention window, and finished jobs older than it (`GM_RETENTION_DAYS`, default
/// 60). `audit_log` is kept.
pub async fn apply_retention(pool: &PgPool, days: i64) -> sqlx::Result<(u64, u64)> {
    let ev =
        sqlx::query("DELETE FROM events WHERE created_at < now() - make_interval(days => $1::int)")
            .bind(days as i32)
            .execute(pool)
            .await?
            .rows_affected();
    let jobs = sqlx::query(
        "DELETE FROM jobs WHERE status IN ('done','dead') AND updated_at < now() - make_interval(days => $1::int)",
    )
    .bind(days as i32)
    .execute(pool)
    .await?
    .rows_affected();
    sqlx::query("DELETE FROM digests WHERE created_at < now() - make_interval(days => $1::int)")
        .bind(days as i32)
        .execute(pool)
        .await?;
    // Tasks hold mail-derived text (approvals go with them); audit_log is kept forever.
    for table in ["tasks", "chat_messages"] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE created_at < now() - make_interval(days => $1::int)"
        ))
        .bind(days as i32)
        .execute(pool)
        .await?;
    }
    sqlx::query("DELETE FROM llm_calls WHERE created_at < now() - make_interval(days => $1::int)")
        .bind(days as i32)
        .execute(pool)
        .await?;
    Ok((ev, jobs))
}

/// A machine-wide setting (`system_settings`), or `None` if never set.
pub async fn system_get(pool: &PgPool, key: &str) -> sqlx::Result<Option<Value>> {
    sqlx::query_scalar("SELECT value FROM system_settings WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
}

pub async fn system_set(pool: &PgPool, key: &str, value: &Value) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO system_settings (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}
