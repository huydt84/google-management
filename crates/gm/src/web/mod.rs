//! Web UI and API (axum). Binds to localhost only; remote access goes through Tailscale.

mod auth;
mod pages;
pub mod session;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum_extra::extract::CookieJar;
use sqlx::PgPool;

use crate::config::Config;
use crate::gmail::ingest::GmailIngestor;
use crate::google::accounts::AccountStore;
use crate::user_settings::UserSettingsStore;
use session::{SESSION_COOKIE, Session, Signer};

pub struct AppState {
    pub cfg: Config,
    pub pool: PgPool,
    pub http: reqwest::Client,
    pub accounts: Arc<AccountStore>,
    pub ingestor: Arc<GmailIngestor>,
    pub user_settings: UserSettingsStore,
    pub signer: Signer,
    pub notifier_name: &'static str,
}

pub type SharedState = Arc<AppState>;

/// Internal errors become a plain 500; details go to the log only.
pub struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!(error = %format!("{:#}", self.0), "request failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
    }
}

/// The signed-in owner, if the session cookie is valid and belongs to `GM_ALLOWED_EMAIL`.
pub fn current_owner(state: &AppState, jar: &CookieJar) -> Option<String> {
    let s: Session = state
        .signer
        .verify("session", jar.get(SESSION_COOKIE)?.value())?;
    let now = chrono::Utc::now().timestamp();
    (s.exp > now && s.email == state.cfg.allowed_email).then_some(s.email)
}

fn host_allowed(cfg: &Config, host: &str) -> bool {
    let name = if host.starts_with('[') {
        host.split(']')
            .next()
            .unwrap_or(host)
            .trim_start_matches('[')
    } else {
        host.split(':').next().unwrap_or(host)
    };
    cfg.allowed_hosts().iter().any(|h| h == name)
}

/// Reject unknown Host headers (DNS rebinding against localhost) and cross-origin POSTs
/// (defense in depth on top of SameSite=Lax cookies).
async fn guard(
    State(state): State<SharedState>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !host_allowed(&state.cfg, host) {
        return (StatusCode::BAD_REQUEST, "Invalid host").into_response();
    }
    if req.method() == Method::POST
        && let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok())
    {
        let ok = url::Url::parse(origin)
            .ok()
            .and_then(|u| {
                u.host_str()
                    .map(|h| state.cfg.allowed_hosts().iter().any(|a| a == h))
            })
            .unwrap_or(false);
        if !ok {
            return (StatusCode::FORBIDDEN, "Cross-origin request rejected").into_response();
        }
    }
    next.run(req).await
}

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(pages::index))
        .route(
            "/settings",
            get(pages::settings_page).post(pages::save_settings),
        )
        .route("/digest", get(pages::digest_page))
        .route("/api/gmail/sync", post(pages::sync_now))
        .route("/api/events", get(pages::events_json))
        .route("/healthz", get(pages::healthz))
        .route("/metrics", get(pages::metrics))
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::login_callback))
        .route("/auth/logout", post(auth::logout))
        .route("/connect/google", get(auth::connect))
        .route("/connect/google/callback", get(auth::connect_callback))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}
