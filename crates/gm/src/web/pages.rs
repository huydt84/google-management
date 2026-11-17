//! Dashboard, settings, JSON API, health and metrics.

use askama::Template;
use axum::Json;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AppError, SharedState, current_owner};
use crate::i18n::{Tr, UI_LANGUAGES};
use crate::user_settings::{COMMON_LANGUAGES, UserSettings};

/// A UTC time shown in the Mac's local time zone.
fn local_time(t: &chrono::DateTime<chrono::Utc>, fmt: &str) -> String {
    t.with_timezone(&chrono::Local).format(fmt).to_string()
}

fn render<T: Template>(t: &T) -> Result<Response, AppError> {
    Ok(Html(t.render()?).into_response())
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    tr: Tr,
}

pub struct EventRow {
    pub id: i64,
    pub time: String,
    pub from: String,
    pub subject: String,
    pub link: String,
    pub status: String,
    pub rule: String,
    /// Model triage reason (`rule = "model"`).
    pub reason: String,
    pub importance: String,
    pub summary: String,
    pub error: String,
    /// The owner's 👍/👎 (`up` / `down`), if any.
    pub feedback: String,
    /// Whether the message is in the local eval set.
    pub in_eval: bool,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    tr: Tr,
    email: String,
    account_status: String,
    history_id: String,
    last_sync: String,
    last_error: String,
    ingest: String,
    ingest_error: String,
    llm_model: String,
    llm_base_url: String,
    model_hours: String,
    summary_language: String,
    notifier: String,
    job_stats: Vec<(String, i64)>,
    health: Vec<HealthRow>,
    events: Vec<EventRow>,
    /// Outcome of "Add to eval": message key, or empty.
    banner: String,
}

pub struct HealthRow {
    pub label: String,
    /// ok | failing | skipped
    pub state: String,
    pub since: String,
}

async fn health_rows(state: &SharedState, tr: &Tr) -> Result<Vec<HealthRow>, AppError> {
    let incidents = state.health.incidents().await?;
    Ok(state
        .health
        .evaluate()
        .await?
        .into_iter()
        .map(|r| {
            let since = incidents
                .iter()
                .find(|(n, _)| n == r.name)
                .map(|(_, t)| local_time(t, "%m-%d %H:%M"))
                .unwrap_or_default();
            HealthRow {
                state: match r.failing {
                    Some(true) => "failing",
                    Some(false) => "ok",
                    None => "skipped",
                }
                .into(),
                label: if r.failing == Some(true) {
                    r.message(tr)
                } else {
                    tr.get(&format!("health-label-{}", r.name.replace('_', "-")))
                },
                since,
            }
        })
        .collect())
}

/// The summary as one line: a notification summary, a digest line, or a job list.
fn summary_text(summary: &Value) -> String {
    if let Some(s) = summary["summary"].as_str().or(summary["line"].as_str()) {
        return s.to_string();
    }
    summary["jobs"]
        .as_array()
        .map(|jobs| {
            jobs.iter()
                .filter_map(|j| j["title"].as_str())
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .unwrap_or_default()
}

fn event_row(e: crate::db::Event) -> EventRow {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let summary = e.summary.clone().unwrap_or(Value::Null);
    EventRow {
        id: e.id,
        time: local_time(&e.created_at, "%m-%d %H:%M"),
        from: s(&e.data["from"]),
        subject: e.data["subject"]
            .as_str()
            .map(str::to_string)
            .unwrap_or(e.external_id.clone()),
        link: s(&e.data["link"]),
        status: e.status,
        rule: e.rule.unwrap_or_default(),
        reason: s(&e.data["verdict"]["reason"]),
        importance: s(&summary["importance"]),
        summary: summary_text(&summary),
        error: s(&summary["error"]),
        feedback: String::new(),
        in_eval: false,
    }
}

async fn with_feedback(
    state: &SharedState,
    events: Vec<crate::db::Event>,
) -> Result<Vec<EventRow>, AppError> {
    let ids: Vec<i64> = events.iter().map(|e| e.id).collect();
    let labels = crate::feedback::labels(&state.pool, &ids).await?;
    let in_eval: std::collections::HashSet<String> =
        crate::eval::ids(&state.cfg.eval_dir).into_iter().collect();
    Ok(events
        .into_iter()
        .map(|e| {
            let fb = labels.get(&e.id).cloned().unwrap_or_default();
            EventRow {
                feedback: fb,
                in_eval: in_eval.contains(&e.external_id),
                ..event_row(e)
            }
        })
        .collect())
}

#[derive(Deserialize, Default)]
pub struct IndexQuery {
    #[serde(default)]
    eval: String,
}

pub async fn index(
    State(state): State<SharedState>,
    jar: CookieJar,
    Query(q): Query<IndexQuery>,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return render(&LoginPage {
            tr: Tr::new(&state.user_settings.defaults().ui_language),
        });
    };
    let prefs = state.user_settings.get(&email).await?;
    let account = state.accounts.get(&email).await?;
    let gs: Option<crate::db::GmailState> = sqlx::query_as(
        "SELECT email, history_id, last_sync_at, last_error FROM gmail_state WHERE email = $1",
    )
    .bind(&email)
    .fetch_optional(&state.pool)
    .await?;
    let tr = Tr::new(&prefs.ui_language);
    let hours = if state.cfg.model_on == state.cfg.model_off {
        tr.get("always")
    } else {
        format!(
            "{}–{}",
            state.cfg.model_on.format("%H:%M"),
            state.cfg.model_off.format("%H:%M")
        )
    };
    let poll_s = state.cfg.gmail_poll_interval_s;
    let (ingest, ingest_error) = match &state.cfg.pubsub {
        None => (
            tr.args("ingest-poll", &[("seconds", poll_s.to_string())]),
            String::new(),
        ),
        Some(ps) if state.ingestor.pubsub_ok() => (
            tr.args(
                "ingest-pubsub-ok",
                &[("minutes", (ps.safety_poll_interval_s / 60).to_string())],
            ),
            String::new(),
        ),
        Some(_) => (
            tr.args("ingest-pubsub-down", &[("seconds", poll_s.to_string())]),
            state.ingestor.pubsub_error().unwrap_or_default(),
        ),
    };
    let page = IndexPage {
        email,
        account_status: account.map(|a| a.status).unwrap_or_default(),
        history_id: gs
            .as_ref()
            .and_then(|g| g.history_id)
            .map(|h| h.to_string())
            .unwrap_or_else(|| "—".into()),
        last_sync: gs
            .as_ref()
            .and_then(|g| g.last_sync_at)
            .map(|t| local_time(&t, "%Y-%m-%d %H:%M:%S"))
            .unwrap_or_else(|| "—".into()),
        last_error: gs.and_then(|g| g.last_error).unwrap_or_default(),
        ingest,
        ingest_error,
        llm_model: state.cfg.llm_model.clone(),
        llm_base_url: state.cfg.llm_base_url.clone(),
        model_hours: hours,
        summary_language: prefs.summary_language,
        notifier: if state.notifier_name == "telegram" {
            tr.get("telegram")
        } else {
            tr.get("log-only")
        },
        job_stats: crate::db::count_by(&state.pool, "jobs").await?,
        health: health_rows(&state, &tr).await?,
        events: with_feedback(&state, crate::db::recent_events(&state.pool, 50).await?).await?,
        banner: match q.eval.as_str() {
            "added" | "updated" | "full" | "gone" => format!("eval-{}", q.eval),
            _ => String::new(),
        },
        tr,
    };
    render(&page)
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    tr: Tr,
    email: String,
    summary_language: String,
    translation_language: String,
    ui_language: String,
    ui_languages: Vec<(String, String)>,
    languages: Vec<String>,
    default_help: String,
    digest_at: String,
    digest_help: String,
    error: String,
    saved: bool,
}

fn settings_page_for(
    state: &SharedState,
    email: String,
    s: &UserSettings,
    error: String,
    saved: bool,
) -> SettingsPage {
    let tr = Tr::new(&s.ui_language);
    SettingsPage {
        default_help: tr.args(
            "summary-language-help",
            &[("default", state.cfg.default_summary_language.clone())],
        ),
        digest_help: tr.args(
            "digest-time-help",
            &[
                ("on", state.cfg.model_on.format("%H:%M").to_string()),
                ("off", state.cfg.model_off.format("%H:%M").to_string()),
            ],
        ),
        digest_at: s.digest_at.clone(),
        tr,
        email,
        summary_language: s.summary_language.clone(),
        translation_language: s.translation_language.clone().unwrap_or_default(),
        ui_language: s.ui_language.clone(),
        ui_languages: UI_LANGUAGES
            .iter()
            .map(|(c, n)| (c.to_string(), n.to_string()))
            .collect(),
        languages: COMMON_LANGUAGES.iter().map(|s| s.to_string()).collect(),
        error,
        saved,
    }
}

#[derive(Deserialize)]
pub struct SavedQuery {
    saved: Option<String>,
}

pub async fn settings_page(
    State(state): State<SharedState>,
    jar: CookieJar,
    Query(q): Query<SavedQuery>,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return Ok(Redirect::to("/").into_response());
    };
    let prefs = state.user_settings.get(&email).await?;
    render(&settings_page_for(
        &state,
        email,
        &prefs,
        String::new(),
        q.saved.as_deref() == Some("1"),
    ))
}

#[derive(Deserialize)]
pub struct SettingsForm {
    summary_language: String,
    #[serde(default)]
    translation_language: String,
    #[serde(default)]
    ui_language: String,
    #[serde(default)]
    digest_at: String,
}

pub async fn save_settings(
    State(state): State<SharedState>,
    jar: CookieJar,
    Form(f): Form<SettingsForm>,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return Ok(Redirect::to("/").into_response());
    };
    let submitted = UserSettings {
        summary_language: f.summary_language,
        translation_language: Some(f.translation_language),
        ui_language: if f.ui_language.is_empty() {
            state.user_settings.defaults().ui_language.clone()
        } else {
            f.ui_language
        },
        digest_at: if f.digest_at.is_empty() {
            state.user_settings.defaults().digest_at.clone()
        } else {
            f.digest_at
        },
    };
    match state.user_settings.save(&email, submitted.clone()).await? {
        Ok(_) => Ok(Redirect::to("/settings?saved=1").into_response()),
        Err(e) => {
            let current = state.user_settings.get(&email).await?;
            let shown = UserSettings {
                ui_language: current.ui_language,
                ..submitted
            };
            let page = settings_page_for(&state, email, &shown, String::new(), false);
            let msg = page.tr.get(e.message_id());
            let page = SettingsPage { error: msg, ..page };
            Ok((StatusCode::BAD_REQUEST, Html(page.render()?)).into_response())
        }
    }
}

pub async fn sync_now(
    State(state): State<SharedState>,
    jar: CookieJar,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return Ok((
            StatusCode::UNAUTHORIZED,
            Json(json!({"detail": "login required"})),
        )
            .into_response());
    };
    if let Err(e) = state.ingestor.sync(&email).await {
        tracing::warn!(error = %e, "manual sync failed");
    }
    Ok(Redirect::to("/").into_response())
}

pub async fn events_json(
    State(state): State<SharedState>,
    jar: CookieJar,
) -> Result<Response, AppError> {
    if current_owner(&state, &jar).is_none() {
        return Ok((
            StatusCode::UNAUTHORIZED,
            Json(json!({"detail": "login required"})),
        )
            .into_response());
    }
    Ok(Json(crate::db::recent_events(&state.pool, 100).await?).into_response())
}

pub async fn healthz(State(state): State<SharedState>) -> Response {
    match sqlx::query("SELECT 1").execute(&state.pool).await {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false}))).into_response(),
    }
}

/// Prometheus text format, behind the `metrics-token` bearer secret (404 when not configured).
pub async fn metrics(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let Some(token) = &state.cfg.metrics_token else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = format!("Bearer {token}");
    let ok = auth.len() == expected.len()
        && auth
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
    if !ok {
        return Ok(StatusCode::UNAUTHORIZED.into_response());
    }
    let mut out = String::new();
    out.push_str("# TYPE gm_events gauge\n");
    for (status, n) in crate::db::count_by(&state.pool, "events").await? {
        out.push_str(&format!("gm_events{{status=\"{status}\"}} {n}\n"));
    }
    out.push_str("# TYPE gm_jobs gauge\n");
    for (status, n) in crate::db::count_by(&state.pool, "jobs").await? {
        out.push_str(&format!("gm_jobs{{status=\"{status}\"}} {n}\n"));
    }
    let rows: Vec<(String, String, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT task, lane, count(*), COALESCE(sum(prompt_tokens), 0)::bigint, COALESCE(sum(completion_tokens), 0)::bigint,
                COALESCE(sum(latency_ms), 0)::bigint
         FROM llm_calls WHERE created_at > now() - interval '24 hours' GROUP BY task, lane ORDER BY task, lane",
    )
    .fetch_all(&state.pool)
    .await?;
    out.push_str("# TYPE gm_llm_calls_24h gauge\n# TYPE gm_llm_prompt_tokens_24h gauge\n");
    out.push_str(
        "# TYPE gm_llm_completion_tokens_24h gauge\n# TYPE gm_llm_latency_ms_sum_24h gauge\n",
    );
    for (task, lane, n, pt, ct, lat) in rows {
        let l = format!("task=\"{task}\",lane=\"{lane}\"");
        out.push_str(&format!(
            "gm_llm_calls_24h{{{l}}} {n}\ngm_llm_prompt_tokens_24h{{{l}}} {pt}\ngm_llm_completion_tokens_24h{{{l}}} {ct}\ngm_llm_latency_ms_sum_24h{{{l}}} {lat}\n"
        ));
    }
    let lag: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM now() - min(last_sync_at))::float8 FROM gmail_state",
    )
    .fetch_one(&state.pool)
    .await?;
    if let Some(lag) = lag {
        out.push_str(&format!(
            "# TYPE gm_ingest_lag_seconds gauge\ngm_ingest_lag_seconds {lag:.0}\n"
        ));
    }
    out.push_str("# TYPE gm_health_failing gauge\n");
    for r in state.health.evaluate().await? {
        if let Some(f) = r.failing {
            out.push_str(&format!(
                "gm_health_failing{{check=\"{}\"}} {}\n",
                r.name,
                u8::from(f)
            ));
        }
    }
    if state.cfg.pubsub.is_some() {
        out.push_str(&format!(
            "# TYPE gm_pubsub_healthy gauge\ngm_pubsub_healthy {}\n",
            u8::from(state.ingestor.pubsub_ok())
        ));
    }
    Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response())
}

pub struct DigestRow {
    pub id: i64,
    pub date: String,
    pub items: i32,
    pub status: String,
}

#[derive(Template)]
#[template(path = "digest.html")]
struct DigestPage {
    tr: Tr,
    email: String,
    digests: Vec<DigestRow>,
    selected_id: i64,
    selected_text: String,
}

#[derive(Deserialize)]
pub struct IdQuery {
    id: Option<i64>,
}

/// Today's and past digests (2A).
pub async fn digest_page(
    State(state): State<SharedState>,
    jar: CookieJar,
    Query(q): Query<IdQuery>,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return Ok(Redirect::to("/").into_response());
    };
    let prefs = state.user_settings.get(&email).await?;
    let rows: Vec<(i64, chrono::NaiveDate, i32, String, String)> = sqlx::query_as(
        "SELECT id, digest_date, item_count, status, text FROM digests
         WHERE owner_email = $1 ORDER BY digest_date DESC LIMIT 60",
    )
    .bind(&email)
    .fetch_all(&state.pool)
    .await?;
    let selected =
        q.id.and_then(|id| rows.iter().find(|r| r.0 == id))
            .or(rows.first());
    let (selected_id, selected_text) = selected
        .map(|r| (r.0, r.4.clone()))
        .unwrap_or((0, String::new()));
    render(&DigestPage {
        tr: Tr::new(&prefs.ui_language),
        email,
        digests: rows
            .iter()
            .map(|(id, date, items, status, _)| DigestRow {
                id: *id,
                date: date.format("%Y-%m-%d").to_string(),
                items: *items,
                status: status.clone(),
            })
            .collect(),
        selected_id,
        selected_text,
    })
}

type JobDbRow = (
    i64,
    String,
    String,
    String,
    i32,
    chrono::DateTime<chrono::Utc>,
    Option<String>,
);
type SenderDbRow = (
    String,
    i64,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
    Option<String>,
);

pub struct JobRow {
    pub id: i64,
    pub topic: String,
    pub subscriber: String,
    pub status: String,
    pub attempts: i32,
    pub updated: String,
    pub error: String,
}

#[derive(Template)]
#[template(path = "jobs.html")]
struct JobsPage {
    tr: Tr,
    email: String,
    status: String,
    statuses: Vec<String>,
    jobs: Vec<JobRow>,
}

#[derive(Deserialize)]
pub struct JobsQuery {
    status: Option<String>,
}

/// Jobs by status, dead letters by default, with Retry (2E, decision N18).
pub async fn jobs_page(
    State(state): State<SharedState>,
    jar: CookieJar,
    Query(q): Query<JobsQuery>,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return Ok(Redirect::to("/").into_response());
    };
    let statuses = ["dead", "pending", "running", "done"];
    let status = q
        .status
        .filter(|s| statuses.contains(&s.as_str()))
        .unwrap_or_else(|| "dead".into());
    let rows: Vec<JobDbRow> = sqlx::query_as(
        "SELECT id, topic, subscriber, status, attempts, updated_at, last_error FROM jobs
             WHERE status = $1 ORDER BY id DESC LIMIT 200",
    )
    .bind(&status)
    .fetch_all(&state.pool)
    .await?;
    let prefs = state.user_settings.get(&email).await?;
    render(&JobsPage {
        tr: Tr::new(&prefs.ui_language),
        email,
        status,
        statuses: statuses.iter().map(|s| s.to_string()).collect(),
        jobs: rows
            .into_iter()
            .map(
                |(id, topic, subscriber, status, attempts, updated, error)| JobRow {
                    id,
                    topic,
                    subscriber,
                    status,
                    attempts,
                    updated: local_time(&updated, "%m-%d %H:%M"),
                    error: error.unwrap_or_default(),
                },
            )
            .collect(),
    })
}

const RETRY_SQL: &str = "UPDATE jobs SET status = 'pending', attempts = 0, run_after = now(),
    last_error = NULL, locked_at = NULL, updated_at = now() WHERE status = 'dead'";

pub async fn retry_job(
    State(state): State<SharedState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Response, AppError> {
    if current_owner(&state, &jar).is_none() {
        return Ok(StatusCode::UNAUTHORIZED.into_response());
    }
    sqlx::query(&format!("{RETRY_SQL} AND id = $1"))
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(Redirect::to("/jobs?status=dead").into_response())
}

pub async fn retry_all_dead(
    State(state): State<SharedState>,
    jar: CookieJar,
) -> Result<Response, AppError> {
    if current_owner(&state, &jar).is_none() {
        return Ok(StatusCode::UNAUTHORIZED.into_response());
    }
    let n = sqlx::query(RETRY_SQL)
        .execute(&state.pool)
        .await?
        .rows_affected();
    tracing::info!(n, "dead jobs retried from the dashboard");
    Ok(Redirect::to("/jobs?status=dead").into_response())
}

pub struct SenderRow {
    pub address: String,
    pub count: i64,
    pub last_subject: String,
    pub last_seen: String,
    pub rules: String,
}

#[derive(Template)]
#[template(path = "senders.html")]
struct SendersPage {
    tr: Tr,
    email: String,
    senders: Vec<SenderRow>,
}

/// Ignored mail of the last 30 days by sender (T15, decision N17).
pub async fn senders_page(
    State(state): State<SharedState>,
    jar: CookieJar,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return Ok(Redirect::to("/").into_response());
    };
    let rows: Vec<SenderDbRow> = sqlx::query_as(
        "SELECT COALESCE(NULLIF(data->>'from_addr', ''), '?') AS addr, count(*),
                    (array_agg(data->>'subject' ORDER BY id DESC))[1], max(created_at),
                    string_agg(DISTINCT COALESCE(rule, '?'), ', ')
             FROM events WHERE status = 'ignored' AND created_at > now() - interval '30 days'
             GROUP BY addr ORDER BY count(*) DESC, addr LIMIT 300",
    )
    .fetch_all(&state.pool)
    .await?;
    let prefs = state.user_settings.get(&email).await?;
    render(&SendersPage {
        tr: Tr::new(&prefs.ui_language),
        email,
        senders: rows
            .into_iter()
            .map(|(address, count, subject, last, rules)| SenderRow {
                address,
                count,
                last_subject: subject.unwrap_or_default(),
                last_seen: local_time(&last, "%m-%d"),
                rules: rules.unwrap_or_default(),
            })
            .collect(),
    })
}

#[derive(Deserialize)]
pub struct FeedbackForm {
    label: String,
}

/// 👍 / 👎 from the dashboard (2C).
pub async fn event_feedback(
    State(state): State<SharedState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Form(f): Form<FeedbackForm>,
) -> Result<Response, AppError> {
    if current_owner(&state, &jar).is_none() {
        return Ok(StatusCode::UNAUTHORIZED.into_response());
    }
    if !matches!(f.label.as_str(), "up" | "down") {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    }
    crate::feedback::record(&state.pool, id, &f.label, "web", chrono::Utc::now()).await?;
    Ok(Redirect::to(&format!("/#event-{id}")).into_response())
}

#[derive(Deserialize)]
pub struct EvalForm {
    expected: String,
}

/// Add the event's mail to the local eval set with the action it should have had (decision N15).
/// The raw message is fetched from Gmail once and written to `GM_EVAL_DIR` (never to the database).
pub async fn event_eval(
    State(state): State<SharedState>,
    jar: CookieJar,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Form(f): Form<EvalForm>,
) -> Result<Response, AppError> {
    if current_owner(&state, &jar).is_none() {
        return Ok(StatusCode::UNAUTHORIZED.into_response());
    }
    let Some(expected) = crate::rules::Action::parse_final(&f.expected) else {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    };
    let back =
        |outcome: &str| Redirect::to(&format!("/?eval={outcome}#event-{id}")).into_response();
    let Some(ev) = crate::db::event(&state.pool, id).await? else {
        return Ok(back("gone"));
    };
    let message = match state.gmail.get_message(&ev.account, &ev.external_id).await {
        Ok(m) => m,
        Err(crate::google::GoogleError::NotFound) => return Ok(back("gone")),
        Err(e) => return Err(e.into()),
    };
    let case = crate::eval::EvalCase {
        message_id: ev.external_id.clone(),
        account: ev.account,
        added_at: chrono::Utc::now(),
        expected,
        notes: String::new(),
        message,
    };
    let dir = state.cfg.eval_dir.clone();
    let added = tokio::task::spawn_blocking(move || crate::eval::add(&dir, &case)).await??;
    tracing::info!(event_id = id, outcome = ?added, "eval case saved");
    Ok(back(match added {
        crate::eval::Added::New => "added",
        crate::eval::Added::Updated => "updated",
        crate::eval::Added::Full => "full",
    }))
}
