//! Per-user routing overrides for the configured Gmail rules.

use askama::Template;
use axum::extract::{Form, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use serde::Deserialize;

use super::{AppError, Nav, SharedState, current_owner};
use crate::i18n::Tr;
use crate::rules::Action;

#[derive(Template)]
#[template(path = "rules.html")]
struct RulesPage {
    tr: Tr,
    nav: Nav,
    email: String,
    rules: Vec<RuleRow>,
    routes: [(&'static str, String); 3],
    fallback: String,
    invalid: bool,
}

struct RuleRow {
    name: String,
    conditions: Vec<(String, String)>,
    default_label: String,
    selected: &'static str,
    saved: bool,
}

#[derive(Deserialize)]
pub(super) struct RulesQuery {
    saved: Option<usize>,
}

pub(super) async fn page(
    State(state): State<SharedState>,
    jar: CookieJar,
    Query(query): Query<RulesQuery>,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return Ok(Redirect::to("/").into_response());
    };
    render_page(&state, email, query.saved, StatusCode::OK).await
}

#[derive(Deserialize)]
pub(super) struct RuleForm {
    rule: String,
    action: String,
}

pub(super) async fn save(
    State(state): State<SharedState>,
    jar: CookieJar,
    Form(form): Form<RuleForm>,
) -> Result<Response, AppError> {
    let Some(email) = current_owner(&state, &jar) else {
        return Ok(Redirect::to("/").into_response());
    };
    let rules = state.rules.get();
    let index = rules.rules.iter().position(|rule| rule.name == form.rule);
    let action = Action::parse_final(&form.action);
    match index {
        Some(index) if form.action.is_empty() || action.is_some() => {
            state
                .user_settings
                .set_rule_route(&email, &form.rule, action)
                .await?;
            let row = index + 1;
            Ok(Redirect::to(&format!("/rules?saved={row}#rule-{row}")).into_response())
        }
        _ => render_page(&state, email, None, StatusCode::BAD_REQUEST).await,
    }
}

fn route_label(tr: &Tr, action: Action) -> String {
    match action {
        Action::Notify => tr.get("rules-notify"),
        Action::Digest => tr.get("rules-digest"),
        Action::Ignore => tr.get("eval-ignore"),
        Action::Triage => tr.get("rules-model-triage"),
    }
}

async fn render_page(
    state: &SharedState,
    email: String,
    saved: Option<usize>,
    status: StatusCode,
) -> Result<Response, AppError> {
    let tr = state.user_settings.tr(&email).await;
    let overrides = state.user_settings.rule_routes(&email).await?;
    let rules = state.rules.get();
    let page = RulesPage {
        nav: Nav::load(state, "rules").await?,
        fallback: route_label(&tr, rules.default),
        routes: Action::FINAL.map(|action| (action.as_str(), route_label(&tr, action))),
        rules: rules
            .rules
            .iter()
            .enumerate()
            .map(|(index, rule)| RuleRow {
                name: rule.name.clone(),
                conditions: rule
                    .conditions()
                    .into_iter()
                    .map(|(key, value)| (tr.get(&format!("rules-match-{key}")), value))
                    .collect(),
                default_label: tr.args(
                    "rules-use-default",
                    &[("route", route_label(&tr, rule.action))],
                ),
                selected: overrides.get(&rule.name).map(Action::as_str).unwrap_or(""),
                saved: saved == Some(index + 1),
            })
            .collect(),
        tr,
        email,
        invalid: status == StatusCode::BAD_REQUEST,
    };
    Ok((status, Html(page.render()?)).into_response())
}
