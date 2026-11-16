//! Two OAuth flows against the same Google client:
//! - `/auth/login`: OpenID sign-in to this UI. Only `GM_ALLOWED_EMAIL` (verified) gets a session.
//! - `/connect/google`: grants API scopes with offline access (refresh token) for background work.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use chrono::Utc;
use serde::Deserialize;

use super::session::{OAUTH_COOKIE, OAuthState, SESSION_COOKIE, Session};
use super::{AppError, SharedState, current_owner};
use crate::google::oauth::{self, Flow};

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

fn cookie(
    state: &SharedState,
    name: &'static str,
    value: String,
    max_age_s: i64,
) -> Cookie<'static> {
    Cookie::build((name, value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax) // cross-site POSTs carry no cookie → CSRF-safe forms
        .secure(state.cfg.base_url.starts_with("https://"))
        .max_age(time_duration(max_age_s))
        .build()
}

fn time_duration(secs: i64) -> time::Duration {
    time::Duration::seconds(secs)
}

fn start(state: &SharedState, jar: CookieJar, flow: Flow) -> (CookieJar, Redirect) {
    let st = OAuthState {
        flow,
        state: oauth::random_token(32),
        verifier: oauth::random_token(64),
        exp: Utc::now().timestamp() + 600,
    };
    let url = oauth::authorize_url(&state.cfg, flow, &st.state, &st.verifier);
    let jar = jar.add(cookie(
        state,
        OAUTH_COOKIE,
        state.signer.sign("oauth", &st),
        600,
    ));
    (jar, Redirect::to(&url))
}

fn forbidden(msg: &str) -> Response {
    (StatusCode::FORBIDDEN, msg.to_string()).into_response()
}

/// Validate state, exchange the code, and verify the identity is the allowed, verified owner.
async fn finish(
    state: &SharedState,
    jar: &CookieJar,
    q: &CallbackQuery,
    flow: Flow,
) -> Result<Result<(String, oauth::TokenResponse), Response>, AppError> {
    if let Some(err) = &q.error {
        return Ok(Err((
            StatusCode::BAD_REQUEST,
            format!("Google returned an error: {err}"),
        )
            .into_response()));
    }
    let saved: Option<OAuthState> = jar
        .get(OAUTH_COOKIE)
        .and_then(|c| state.signer.verify("oauth", c.value()));
    let Some(saved) = saved else {
        return Ok(Err(forbidden("Sign-in expired; please try again.")));
    };
    if saved.flow != flow
        || saved.exp < Utc::now().timestamp()
        || q.state.as_deref() != Some(saved.state.as_str())
    {
        return Ok(Err(forbidden("Invalid OAuth state; please try again.")));
    }
    let Some(code) = &q.code else {
        return Ok(Err(
            (StatusCode::BAD_REQUEST, "Missing code").into_response()
        ));
    };
    let tokens = oauth::exchange_code(&state.http, &state.cfg, flow, code, &saved.verifier).await?;
    let id_token = tokens
        .id_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("no id_token in token response"))?;
    let claims = oauth::verify_id_token(
        id_token,
        &state.cfg.google_client_id,
        Utc::now().timestamp(),
    )?;
    let email = claims.email.to_lowercase();
    if !claims.email_verified || email != state.cfg.allowed_email {
        tracing::warn!("rejected Google sign-in for a non-owner account");
        return Ok(Err(forbidden("This Google account is not allowed.")));
    }
    Ok(Ok((email, tokens)))
}

pub async fn login(State(state): State<SharedState>, jar: CookieJar) -> impl IntoResponse {
    start(&state, jar, Flow::Login)
}

pub async fn login_callback(
    State(state): State<SharedState>,
    jar: CookieJar,
    Query(q): Query<CallbackQuery>,
) -> Result<Response, AppError> {
    let (email, _) = match finish(&state, &jar, &q, Flow::Login).await? {
        Ok(v) => v,
        Err(resp) => return Ok(resp),
    };
    let now = Utc::now().timestamp();
    let session = Session {
        email,
        iat: now,
        exp: now + state.cfg.session_max_age_s,
    };
    let jar = jar
        .remove(Cookie::build(OAUTH_COOKIE).path("/"))
        .add(cookie(
            &state,
            SESSION_COOKIE,
            state.signer.sign("session", &session),
            state.cfg.session_max_age_s,
        ));
    Ok((jar, Redirect::to("/")).into_response())
}

pub async fn logout(jar: CookieJar) -> impl IntoResponse {
    (
        jar.remove(Cookie::build(SESSION_COOKIE).path("/")),
        Redirect::to("/"),
    )
}

pub async fn connect(State(state): State<SharedState>, jar: CookieJar) -> Response {
    if current_owner(&state, &jar).is_none() {
        return Redirect::to("/").into_response();
    }
    start(&state, jar, Flow::Connect).into_response()
}

pub async fn connect_callback(
    State(state): State<SharedState>,
    jar: CookieJar,
    Query(q): Query<CallbackQuery>,
) -> Result<Response, AppError> {
    let Some(owner) = current_owner(&state, &jar) else {
        return Ok(Redirect::to("/").into_response());
    };
    let (email, tokens) = match finish(&state, &jar, &q, Flow::Connect).await? {
        Ok(v) => v,
        Err(resp) => return Ok(resp),
    };
    if email != owner {
        return Ok(forbidden(
            "The connected account differs from the signed-in account.",
        ));
    }
    let Some(refresh) = tokens.refresh_token else {
        return Ok((
            StatusCode::BAD_REQUEST,
            "Google did not return a refresh token. Remove the app at https://myaccount.google.com/permissions and connect again.",
        )
            .into_response());
    };
    let granted: Vec<String> = tokens
        .scope
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let missing: Vec<&str> = state
        .cfg
        .required_scopes()
        .into_iter()
        .filter(|s| !granted.iter().any(|g| g == s))
        .collect();
    if !missing.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            format!("Missing scopes (untick nothing on the consent screen): {missing:?}"),
        )
            .into_response());
    }
    state
        .accounts
        .save(&owner, &email, &refresh, &granted)
        .await?;
    state
        .ingestor
        .bootstrap(&email)
        .await
        .map_err(anyhow::Error::from)?;
    let jar = jar.remove(Cookie::build(OAUTH_COOKIE).path("/"));
    Ok((jar, Redirect::to("/")).into_response())
}
