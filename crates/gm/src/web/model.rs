//! Owner controls for the local model, its inference limits and the evaluation report.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use chrono::NaiveTime;
use serde::Deserialize;

use super::{AppError, Nav, SharedState, current_owner};
use crate::eval::{self, LatestReport};
use crate::i18n::Tr;
use crate::llm::TokenBudget;
use crate::model::{MAX_EDITABLE_TOKENS, ModelSettings, SettingsError};

#[derive(Default)]
pub(crate) struct Evaluation {
    lock: Arc<tokio::sync::Mutex<()>>,
    failed: AtomicBool,
}

#[derive(Template)]
#[template(path = "model.html")]
struct ModelPage {
    tr: Tr,
    email: String,
    nav: Nav,
    settings: ModelSettings,
    on: String,
    off: String,
    served: String,
    status: &'static str,
    status_label: String,
    hours: String,
    ready: bool,
    resting: bool,
    local: bool,
    client: String,
    budgets: Vec<TokenBudget>,
    input_tokens: usize,
    summary_tokens: u32,
    max_tokens: u32,
    inflight: usize,
    input_default: usize,
    summary_default: u32,
    notice: String,
    success: bool,
    notice_field: String,
    evaluation: EvalView,
}

struct EvalView {
    local: bool,
    cases: usize,
    running: bool,
    failed: bool,
    read_failed: bool,
    report: Option<LatestReport>,
}

// Probe first so the displayed status and the served model come from the same readiness check.
pub(super) async fn runtime_state(state: &SharedState) -> (&'static str, bool) {
    let ready = state.model.ready().await;
    let status = match (state.model.resting(), ready) {
        (true, true) => "rest-pending",
        (true, false) => "resting",
        (false, true) => "running",
        (false, false) => "stopped",
    };
    (status, ready)
}

#[derive(Default, Deserialize)]
pub(super) struct ModelQuery {
    #[serde(default)]
    notice: String,
    #[serde(default)]
    field: String,
}

pub(super) async fn page(
    State(state): State<SharedState>,
    jar: CookieJar,
    Query(query): Query<ModelQuery>,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return Ok(Redirect::to("/").into_response());
    };
    let tr = state.user_settings.tr(&email).await;
    let (status, ready) = runtime_state(&state).await;
    let settings = state.model.settings();
    let served = state.model.served_model().unwrap_or_default();
    let active_model = state.model.request_model();
    let (notice, success) = match query.notice.as_str() {
        "model-setting-saved" | "model-power-started" | "model-power-stopped" => {
            (tr.get(&query.notice), true)
        }
        "model-invalid-time"
        | "model-hours-digest"
        | "model-invalid-idle"
        | "model-invalid-grace"
        | "model-invalid-model"
        | "model-invalid-tokens"
        | "model-power-stop-failed"
        | "model-power-start-failed"
        | "model-eval-empty"
        | "model-eval-off"
        | "model-eval-failed"
        | "model-eval-local" => (tr.get(&query.notice), false),
        _ => (String::new(), false),
    };
    let nav = Nav::load(&state, "model").await?;
    let running = state.evaluation.lock.try_lock().is_err();
    let dir = state.cfg.eval_dir.clone();
    let (cases, report) =
        tokio::task::spawn_blocking(move || (eval::ids(&dir).len(), eval::latest_report(&dir)))
            .await?;
    let read_failed = report.is_err();
    let report = report.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "could not read the model evaluation report");
        None
    });
    let evaluation = EvalView {
        local: eval::ensure_local(&state.cfg.llm_base_url).is_ok(),
        cases,
        running,
        failed: state.evaluation.failed.load(Ordering::Relaxed),
        read_failed,
        report,
    };
    let page = ModelPage {
        status,
        status_label: tr.get(&format!("model-state-{status}")),
        ready,
        resting: state.model.resting(),
        local: state.model.is_local(),
        hours: settings.hours().label(&tr.get("always")),
        on: settings.on.format("%H:%M").to_string(),
        off: settings.off.format("%H:%M").to_string(),
        settings,
        served,
        client: state.llm.client_name(&active_model),
        budgets: state.llm.token_budgets(&active_model),
        input_tokens: state.llm.input_tokens(),
        summary_tokens: state.llm.summary_tokens(),
        max_tokens: MAX_EDITABLE_TOKENS,
        inflight: state.llm.concurrency(),
        input_default: state.cfg.llm_max_input_tokens,
        summary_default: state.cfg.llm_summary_max_tokens,
        notice,
        success,
        notice_field: query.field,
        evaluation,
        tr,
        email,
        nav,
    };
    Ok(Html(page.render()?).into_response())
}

#[derive(Deserialize)]
#[serde(tag = "setting", rename_all = "kebab-case")]
pub(super) enum Edit {
    Checkpoint { model: String },
    Hours { on: String, off: String },
    Automatic { manage: Option<String> },
    Idle { minutes: String },
    Grace { seconds: String },
    InputTokens { tokens: String },
    SummaryTokens { tokens: String },
}

impl Edit {
    fn field(&self) -> &'static str {
        match self {
            Self::Checkpoint { .. } => "checkpoint",
            Self::Hours { .. } => "hours",
            Self::Automatic { .. } => "automatic",
            Self::Idle { .. } => "idle",
            Self::Grace { .. } => "grace",
            Self::InputTokens { .. } => "input-tokens",
            Self::SummaryTokens { .. } => "summary-tokens",
        }
    }

    fn apply(self, s: &mut ModelSettings, digest_at: &str) -> Result<(), SettingsError> {
        let time = |value: &str| {
            NaiveTime::parse_from_str(value.trim(), "%H:%M").map_err(|_| SettingsError::Time)
        };
        let tokens = |value: &str| -> Result<Option<u32>, SettingsError> {
            let value = value.trim();
            if value.is_empty() {
                return Ok(None);
            }
            value.parse().map(Some).map_err(|_| SettingsError::Tokens)
        };
        match self {
            Self::Checkpoint { model } => s.model = model.trim().into(),
            Self::Hours { on, off } => {
                (s.on, s.off) = (time(&on)?, time(&off)?);
                if !crate::user_settings::parse_digest_at(digest_at)
                    .is_some_and(|t| s.hours().contains(t))
                {
                    return Err(SettingsError::DigestTime);
                }
            }
            Self::Automatic { manage } => s.manage = manage.is_some(),
            Self::Idle { minutes } => {
                s.idle_min = minutes.parse().map_err(|_| SettingsError::IdleMinutes)?
            }
            Self::Grace { seconds } => {
                s.grace_s = seconds.parse().map_err(|_| SettingsError::Grace)?
            }
            Self::InputTokens { tokens: value } => {
                s.input_tokens = tokens(&value)?.map(|n| n as usize)
            }
            Self::SummaryTokens { tokens: value } => s.summary_tokens = tokens(&value)?,
        }
        Ok(())
    }
}

fn back(field: &str, notice: &str) -> Response {
    Redirect::to(&format!("/model?field={field}&notice={notice}#{field}")).into_response()
}

pub(super) async fn save(
    State(state): State<SharedState>,
    jar: CookieJar,
    Form(edit): Form<Edit>,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return Ok(Redirect::to("/").into_response());
    };
    let field = edit.field();
    if matches!(
        edit,
        Edit::Automatic { .. } | Edit::Checkpoint { .. } | Edit::Idle { .. } | Edit::Grace { .. }
    ) && !state.model.is_local()
    {
        return Ok(Redirect::to("/model#runtime").into_response());
    }
    let digest_at = state.user_settings.get(&email).await?.digest_at;
    let notice = match state.model.update(|s| edit.apply(s, &digest_at)).await? {
        Ok(()) => "model-setting-saved",
        Err(e) => e.message_id(),
    };
    Ok(back(field, notice))
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PowerAction {
    Rest,
    Wakeup,
}

#[derive(Deserialize)]
pub(super) struct PowerForm {
    action: PowerAction,
}

pub(super) async fn power(
    State(state): State<SharedState>,
    jar: CookieJar,
    Form(form): Form<PowerForm>,
) -> Response {
    if current_owner(&state, &jar).is_none() {
        return Redirect::to("/").into_response();
    }
    let (result, success, failure) = match form.action {
        PowerAction::Rest => (
            state.model.rest().await,
            "model-power-stopped",
            "model-power-stop-failed",
        ),
        PowerAction::Wakeup => (
            state.model.wakeup().await,
            "model-power-started",
            "model-power-start-failed",
        ),
    };
    match result {
        Ok(()) => back("runtime", success),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "web model power action failed");
            back("runtime", failure)
        }
    }
}

pub(super) async fn evaluate(
    State(state): State<SharedState>,
    jar: CookieJar,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return Ok(Redirect::to("/").into_response());
    };
    if eval::ensure_local(&state.cfg.llm_base_url).is_err() {
        return Ok(back("evaluation", "model-eval-local"));
    }
    let Ok(run) = state.evaluation.lock.clone().try_lock_owned() else {
        return Ok(Redirect::to("/model#evaluation").into_response());
    };
    let path = state.cfg.eval_dir.clone();
    let cases = tokio::task::spawn_blocking(move || eval::load(&path)).await??;
    if cases.is_empty() {
        return Ok(back("evaluation", "model-eval-empty"));
    }
    if state.model.resting() || !state.model.ready().await {
        return Ok(back("evaluation", "model-eval-off"));
    }
    let language = state.user_settings.get(&email).await?.summary_language;
    let model = state.model.request_model();
    let mut llm = state.llm.for_evaluation(model.clone());
    let calls = llm.record_calls();
    let rules = state.rules.get();
    let dir = state.cfg.eval_dir.clone();
    state.evaluation.failed.store(false, Ordering::Relaxed);
    tokio::spawn(async move {
        let result = async {
            let report = eval::run(&llm, &calls, &rules, &cases, &language).await?;
            tokio::task::spawn_blocking(move || eval::save_report(&dir, &model, &report)).await?
        }
        .await;
        if let Err(e) = result {
            state.evaluation.failed.store(true, Ordering::Relaxed);
            tracing::warn!(error = %format!("{e:#}"), "model evaluation failed");
        }
        drop(run);
    });
    Ok(Redirect::to("/model#evaluation").into_response())
}
