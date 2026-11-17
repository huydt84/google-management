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
    events: Vec<EventRow>,
}

fn event_row(e: crate::db::Event) -> EventRow {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let summary = e.summary.clone().unwrap_or(Value::Null);
    EventRow {
        id: e.id,
        time: e
            .created_at
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string(),
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
        summary: s(&summary["summary"]),
        error: s(&summary["error"]),
    }
}

pub async fn index(State(state): State<SharedState>, jar: CookieJar) -> Result<Response, AppError> {
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
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
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
        events: crate::db::recent_events(&state.pool, 50)
            .await?
            .into_iter()
            .map(event_row)
            .collect(),
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
    if state.cfg.pubsub.is_some() {
        out.push_str(&format!(
            "# TYPE gm_pubsub_healthy gauge\ngm_pubsub_healthy {}\n",
            u8::from(state.ingestor.pubsub_ok())
        ));
    }
    Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response())
}
