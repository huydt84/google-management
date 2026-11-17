//! Tasks page (Phase 3): recent owner tasks, pending approvals with their full preview and
//! Approve / Reject, and the audit log. A web decision goes through the same `approval.decision`
//! handler as a Telegram tap.

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{AppError, Nav, SharedState, current_owner};
use crate::i18n::Tr;
use crate::tasks::ApprovalDecision;

fn local_time(t: &DateTime<Utc>) -> String {
    t.with_timezone(&chrono::Local)
        .format("%m-%d %H:%M")
        .to_string()
}

pub struct TaskRow {
    pub id: i64,
    pub time: String,
    pub intent: String,
    pub status: String,
    pub subject: String,
    pub instruction: String,
    pub error: String,
}

pub struct ApprovalRow {
    pub id: i64,
    pub task_id: i64,
    pub action: String,
    pub preview: String,
    pub expires: String,
}

pub struct AuditRow {
    pub time: String,
    pub action: String,
    pub target: String,
    pub outcome: String,
    pub details: String,
}

#[derive(Template)]
#[template(path = "tasks.html")]
struct TasksPage {
    tr: Tr,
    email: String,
    nav: Nav,
    approvals: Vec<ApprovalRow>,
    tasks: Vec<TaskRow>,
    audit: Vec<AuditRow>,
}

type TaskDbRow = (
    i64,
    DateTime<Utc>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub async fn tasks_page(
    State(state): State<SharedState>,
    jar: CookieJar,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return Ok(Redirect::to("/").into_response());
    };
    let tasks: Vec<TaskDbRow> = sqlx::query_as(
        "SELECT t.id, t.created_at, t.intent, t.status, e.data->>'subject', t.instruction, t.error
         FROM tasks t LEFT JOIN events e ON e.id = t.event_id ORDER BY t.id DESC LIMIT 50",
    )
    .fetch_all(&state.pool)
    .await?;
    let audit: Vec<(DateTime<Utc>, String, String, Value)> = sqlx::query_as(
        "SELECT ts, action, target, details FROM audit_log ORDER BY id DESC LIMIT 50",
    )
    .fetch_all(&state.pool)
    .await?;
    let approvals = crate::tasks::approvals::pending(&state.pool).await?;
    let prefs = state.user_settings.get(&email).await?;
    let page = TasksPage {
        tr: Tr::new(&prefs.ui_language),
        email,
        nav: Nav {
            active: "tasks",
            approvals: approvals.len(),
        },
        approvals: approvals
            .into_iter()
            .map(|a| ApprovalRow {
                id: a.id,
                task_id: a.task_id,
                action: a.action,
                preview: a.preview,
                expires: local_time(&a.expires_at),
            })
            .collect(),
        tasks: tasks
            .into_iter()
            .map(
                |(id, created, intent, status, subject, instruction, error)| TaskRow {
                    id,
                    time: local_time(&created),
                    intent: intent.unwrap_or_default(),
                    status,
                    subject: subject.unwrap_or_default(),
                    instruction: instruction.unwrap_or_default(),
                    error: error.unwrap_or_default(),
                },
            )
            .collect(),
        audit: audit
            .into_iter()
            .map(|(ts, action, target, details)| AuditRow {
                time: local_time(&ts),
                action,
                target,
                outcome: details["outcome"].as_str().unwrap_or_default().to_string(),
                details: details["subject"]
                    .as_str()
                    .or(details["error"].as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect(),
    };
    Ok(axum::response::Html(page.render()?).into_response())
}

async fn decide(state: &SharedState, id: i64, approve: bool) -> Result<(), AppError> {
    let d = ApprovalDecision {
        approval_id: id,
        approve,
        via: "web".into(),
    };
    let mut tx = state.pool.begin().await?;
    state
        .bus
        .publish(
            &mut tx,
            crate::topics::APPROVAL_DECISION,
            &serde_json::to_value(&d)?,
        )
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn approve(
    State(state): State<SharedState>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    if current_owner(&state, &jar).is_none() {
        return Ok(StatusCode::UNAUTHORIZED.into_response());
    }
    decide(&state, id, true).await?;
    Ok(Redirect::to("/tasks").into_response())
}

pub async fn reject(
    State(state): State<SharedState>,
    jar: CookieJar,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    if current_owner(&state, &jar).is_none() {
        return Ok(StatusCode::UNAUTHORIZED.into_response());
    }
    decide(&state, id, false).await?;
    Ok(Redirect::to("/tasks").into_response())
}
