//! Configuration: non-secret values from `GM_*` environment variables (or `.env`), secrets from the
//! Keychain via [`crate::secrets`]. See `docs/spec/interfaces.md#configuration`.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, bail};
use chrono::NaiveTime;

use crate::secrets;

/// Gmail push notifications through Cloud Pub/Sub, pulled by gm (no public webhook). gm creates the
/// topic, subscription and publisher permission itself in `project_id`.
#[derive(Clone, Debug)]
pub struct PubSubConfig {
    pub project_id: String,
    pub topic: String,
    pub subscription: String,
    pub api_base: String,
    /// Poll interval while Pub/Sub works; the normal poll interval applies while it doesn't.
    pub safety_poll_interval_s: u64,
}

impl PubSubConfig {
    pub fn topic_name(&self) -> String {
        format!("projects/{}/topics/{}", self.project_id, self.topic)
    }
    pub fn subscription_name(&self) -> String {
        format!(
            "projects/{}/subscriptions/{}",
            self.project_id, self.subscription
        )
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    // Web (bind to localhost only; remote access goes through Tailscale)
    pub host: String,
    pub port: u16,
    pub base_url: String,
    /// Tailnet URL from `tailscale serve`, accepted as a Host header. Optional.
    pub public_url: Option<String>,
    pub session_max_age_s: i64,

    pub allowed_email: String,
    pub database_url: String,

    // Google OAuth client (type: Web application)
    pub google_client_id: String,
    pub google_client_secret: String,
    pub google_auth_url: String,
    pub google_token_url: String,
    pub gmail_api_base: String,

    // Gmail ingestion
    pub gmail_poll_interval_s: u64,
    pub gmail_label_ids: Vec<String>,
    /// `Some` when `GM_GMAIL_INGEST_MODE=pubsub` (ADR-0002): Pub/Sub pull plus a safety poll.
    pub pubsub: Option<PubSubConfig>,

    pub rules_path: PathBuf,

    // LLM (OpenAI-compatible: mlx-server-runtime)
    pub llm_base_url: String,
    /// mlx-server-runtime readiness probe (`GET`, 2xx = model loaded). Default: the base URL
    /// without `/v1`, plus `/ready`.
    pub llm_ready_url: String,
    pub llm_api_key: String,
    pub llm_model: String,
    pub llm_timeout_s: u64,
    pub llm_max_inflight: usize,
    pub llm_max_input_tokens: usize,
    pub llm_summary_max_tokens: u32,
    /// Appended to prompts of non-thinking tasks (Qwen3: `/no_think`). Empty disables.
    pub llm_no_think_tag: String,
    /// While the model is unavailable, queued summaries try again after this many seconds (L12).
    pub llm_unavailable_retry_s: u64,
    /// Model residency window in local time (ADR-0010). Equal values mean "always on".
    pub model_on: NaiveTime,
    pub model_off: NaiveTime,
    /// Default morning digest time; each user can change it in Settings (decision N4).
    pub digest_at: NaiveTime,

    // Defaults for users who have not saved settings
    pub default_summary_language: String,
    pub default_ui_language: String,

    // Notifications
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub telegram_api_base: String,

    // Background processing
    pub run_background: bool,
    pub worker_concurrency: usize,
    pub job_max_attempts: i32,
    pub retention_days: i64,

    // Secrets
    pub master_key: String,
    pub session_secret: String,
    pub metrics_token: Option<String>,

    pub log_dir: Option<PathBuf>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(format!("GM_{name}"))
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_or(name: &str, default: &str) -> String {
    env(name).unwrap_or_else(|| default.to_string())
}

fn parse<T: FromStr>(name: &str, default: T) -> anyhow::Result<T>
where
    T::Err: std::fmt::Display,
{
    match env(name) {
        None => Ok(default),
        Some(v) => v
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("GM_{name}={v:?}: {e}")),
    }
}

fn required(name: &str) -> anyhow::Result<String> {
    env(name).with_context(|| format!("GM_{name} is required (see .env.example)"))
}

fn secret(name: &str) -> anyhow::Result<String> {
    secrets::get(name).with_context(|| {
        format!(
            "secret {name:?} not found: run `gm keychain set {name}` (or set {})",
            secrets::env_name(name)
        )
    })
}

fn time(name: &str, default: &str) -> anyhow::Result<NaiveTime> {
    let v = env_or(name, default);
    NaiveTime::parse_from_str(&v, "%H:%M")
        .with_context(|| format!("GM_{name}={v:?}: expected HH:MM"))
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let _ = dotenvy::dotenv();
        let base_url = env_or("BASE_URL", "http://localhost:8080")
            .trim_end_matches('/')
            .to_string();
        let host = env_or("HOST", "127.0.0.1");
        if host != "127.0.0.1" && host != "localhost" && host != "::1" {
            bail!(
                "GM_HOST must be a loopback address (got {host:?}); use Tailscale for remote access"
            );
        }
        let llm_base_url = env_or("LLM_BASE_URL", "http://127.0.0.1:8000/v1")
            .trim_end_matches('/')
            .to_string();
        let llm_ready_url = env("LLM_READY_URL")
            .unwrap_or_else(|| format!("{}/ready", llm_base_url.trim_end_matches("/v1")));
        let cfg = Self {
            host,
            port: parse("PORT", 8080)?,
            base_url,
            public_url: env("PUBLIC_URL").map(|u| u.trim_end_matches('/').to_string()),
            session_max_age_s: parse("SESSION_MAX_AGE_S", 7 * 24 * 3600)?,
            allowed_email: required("ALLOWED_EMAIL")?.to_lowercase(),
            database_url: env_or("DATABASE_URL", "postgres://gm:gm@127.0.0.1:5433/gm"),
            google_client_id: required("GOOGLE_CLIENT_ID")?,
            google_client_secret: secret("google-client-secret")?,
            google_auth_url: env_or(
                "GOOGLE_AUTH_URL",
                "https://accounts.google.com/o/oauth2/v2/auth",
            ),
            google_token_url: env_or("GOOGLE_TOKEN_URL", "https://oauth2.googleapis.com/token"),
            gmail_api_base: env_or("GMAIL_API_BASE", "https://gmail.googleapis.com/gmail/v1"),
            gmail_poll_interval_s: parse("GMAIL_POLL_INTERVAL_S", 60)?,
            gmail_label_ids: env_or("GMAIL_LABEL_IDS", "INBOX")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            pubsub: match env_or("GMAIL_INGEST_MODE", "poll").as_str() {
                "poll" => None,
                "pubsub" => Some(PubSubConfig {
                    project_id: required("GOOGLE_PROJECT_ID")?,
                    topic: env_or("GMAIL_PUBSUB_TOPIC", "gm-gmail"),
                    subscription: env_or("GMAIL_PUBSUB_SUBSCRIPTION", "gm-gmail-pull"),
                    api_base: env_or("PUBSUB_API_BASE", "https://pubsub.googleapis.com/v1")
                        .trim_end_matches('/')
                        .to_string(),
                    safety_poll_interval_s: parse("GMAIL_SAFETY_POLL_INTERVAL_S", 900)?,
                }),
                other => bail!("GM_GMAIL_INGEST_MODE must be poll or pubsub (got {other:?})"),
            },
            rules_path: PathBuf::from(env_or("RULES_PATH", "rules.yaml")),
            llm_base_url,
            llm_ready_url,
            llm_api_key: env_or("LLM_API_KEY", "not-needed"),
            llm_model: env_or("LLM_MODEL", "mlx-community/Qwen3-8B-4bit"),
            llm_timeout_s: parse("LLM_TIMEOUT_S", 120)?,
            llm_max_inflight: parse("LLM_MAX_INFLIGHT", 2)?,
            llm_max_input_tokens: parse("LLM_MAX_INPUT_TOKENS", 2000)?,
            llm_summary_max_tokens: parse("LLM_SUMMARY_MAX_TOKENS", 400)?,
            llm_no_think_tag: std::env::var("GM_LLM_NO_THINK_TAG")
                .unwrap_or_else(|_| "/no_think".into()),
            llm_unavailable_retry_s: parse("LLM_UNAVAILABLE_RETRY_S", 300)?,
            model_on: time("MODEL_ON", "09:00")?,
            model_off: time("MODEL_OFF", "19:00")?,
            digest_at: time("DIGEST_AT", "09:05")?,
            default_summary_language: env_or("DEFAULT_SUMMARY_LANGUAGE", "Vietnamese"),
            default_ui_language: env_or("DEFAULT_UI_LANGUAGE", "en"),
            telegram_bot_token: secrets::get("telegram-bot-token"),
            telegram_chat_id: env("TELEGRAM_CHAT_ID"),
            telegram_api_base: env_or("TELEGRAM_API_BASE", "https://api.telegram.org"),
            run_background: parse("RUN_BACKGROUND", true)?,
            worker_concurrency: parse("WORKER_CONCURRENCY", 2)?,
            job_max_attempts: parse("JOB_MAX_ATTEMPTS", 5)?,
            retention_days: parse("RETENTION_DAYS", 60)?,
            master_key: secret("master-key")?,
            session_secret: secret("session-secret")?,
            metrics_token: secrets::get("metrics-token"),
            log_dir: env("LOG_DIR").map(PathBuf::from),
        };
        Ok(cfg)
    }

    /// A complete configuration for tests: nothing points at real services.
    pub fn for_tests() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 0,
            base_url: "http://localhost:8080".into(),
            public_url: None,
            session_max_age_s: 3600,
            allowed_email: "owner@example.com".into(),
            database_url: String::new(),
            google_client_id: "client-id".into(),
            google_client_secret: "client-secret".into(),
            google_auth_url: "http://127.0.0.1:9/auth".into(),
            google_token_url: "http://127.0.0.1:9/token".into(),
            gmail_api_base: "http://127.0.0.1:9/gmail/v1".into(),
            gmail_poll_interval_s: 60,
            gmail_label_ids: vec!["INBOX".into()],
            pubsub: None,
            rules_path: PathBuf::from("does-not-exist.yaml"),
            llm_base_url: "http://127.0.0.1:9/v1".into(),
            llm_ready_url: "http://127.0.0.1:9/ready".into(),
            llm_api_key: "test".into(),
            llm_model: "test-model".into(),
            llm_timeout_s: 5,
            llm_max_inflight: 2,
            llm_max_input_tokens: 2000,
            llm_summary_max_tokens: 400,
            llm_no_think_tag: "/no_think".into(),
            llm_unavailable_retry_s: 300,
            model_on: NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
            model_off: NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
            digest_at: NaiveTime::from_hms_opt(9, 5, 0).unwrap(),
            default_summary_language: "Vietnamese".into(),
            default_ui_language: "en".into(),
            telegram_bot_token: None,
            telegram_chat_id: None,
            telegram_api_base: "http://127.0.0.1:9".into(),
            run_background: false,
            worker_concurrency: 1,
            job_max_attempts: 3,
            retention_days: 60,
            master_key: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=".into(),
            session_secret: "test-session-secret-test-session-secret".into(),
            metrics_token: None,
            log_dir: None,
        }
    }

    /// Google API scopes the Connect flow requests and requires (invariant 4: minimal scopes).
    pub fn required_scopes(&self) -> Vec<&'static str> {
        let mut scopes = vec![crate::google::GMAIL_READONLY];
        if self.pubsub.is_some() {
            scopes.push(crate::google::PUBSUB);
        }
        scopes
    }

    pub fn model_hours(&self) -> crate::clock::ModelHours {
        crate::clock::ModelHours {
            on: self.model_on,
            off: self.model_off,
        }
    }

    pub fn telegram_enabled(&self) -> bool {
        self.telegram_bot_token.is_some() && self.telegram_chat_id.is_some()
    }

    /// Host names accepted in the `Host` header (blocks DNS rebinding against localhost).
    pub fn allowed_hosts(&self) -> Vec<String> {
        let mut hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        for u in [Some(&self.base_url), self.public_url.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Some(h) = url::Url::parse(u)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
            {
                hosts.push(h);
            }
        }
        hosts.sort();
        hosts.dedup();
        hosts
    }
}
