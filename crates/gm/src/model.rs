//! Model residency (ADR-0010): gm decides when the model runs, mlx-air runs it.
//!
//! Inside model hours the model is up. Outside them it is stopped, and an owner request (a
//! Telegram task, a web request) starts it and keeps it up until it has been idle for
//! `idle_min` minutes. gm only calls the `mlx-air` CLI (`serve --detach`, `stop`); the process,
//! its launchd agent and its limits stay mlx-air's. The settings live in `system_settings`
//! (key `model_control`, edited on the dashboard); `.env` gives the first defaults.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::clock::{Clock, ModelHours};
use crate::config::Config;
use crate::llm::{Lane, LlmUnavailable};

/// `system_settings` key holding [`ModelSettings`].
pub const SETTINGS_KEY: &str = "model_control";
/// How often the loop compares the wanted state with the real one.
const RECONCILE_EVERY: Duration = Duration::from_secs(30);
/// A failed start is not retried sooner than this.
const START_RETRY: chrono::Duration = chrono::Duration::seconds(60);

/// The editable part of the model control.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSettings {
    /// Model hours start (local time). `on == off` means always on.
    pub on: NaiveTime,
    pub off: NaiveTime,
    /// `false`: gm leaves the model process alone.
    pub manage: bool,
    /// Minutes the model stays up after the last owner request outside model hours.
    pub idle_min: u32,
    /// Seconds to wait for in-flight requests before stopping the model.
    pub grace_s: u32,
    /// The model id: sent on every request and passed to `mlx-air serve --model`.
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    #[error("idle minutes must be 1–720")]
    IdleMinutes,
    #[error("stop grace must be 0–1800 seconds")]
    Grace,
    #[error("invalid model id")]
    Model,
}

impl SettingsError {
    /// Fluent message id for the error.
    pub fn message_id(&self) -> &'static str {
        match self {
            SettingsError::IdleMinutes => "model-invalid-idle",
            SettingsError::Grace => "model-invalid-grace",
            SettingsError::Model => "model-invalid-model",
        }
    }
}

impl ModelSettings {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            on: cfg.model_on,
            off: cfg.model_off,
            manage: cfg.model_manage,
            idle_min: cfg.model_idle_min,
            grace_s: cfg.model_stop_grace_s,
            model: cfg.llm_model.clone(),
        }
    }

    pub fn hours(&self) -> ModelHours {
        ModelHours {
            on: self.on,
            off: self.off,
        }
    }

    /// The settings in force: the dashboard value if one was saved, else the `.env` defaults.
    pub async fn load(pool: &PgPool, cfg: &Config) -> sqlx::Result<Self> {
        let saved = crate::db::system_get(pool, SETTINGS_KEY).await?;
        Ok(saved
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_else(|| Self::from_config(cfg)))
    }

    pub fn validate(&self) -> Result<(), SettingsError> {
        if !(1..=720).contains(&self.idle_min) {
            return Err(SettingsError::IdleMinutes);
        }
        if self.grace_s > 1800 {
            return Err(SettingsError::Grace);
        }
        let m = self.model.trim();
        if m.is_empty() || m.len() > 200 || m.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(SettingsError::Model);
        }
        Ok(())
    }
}

/// What the model server reports on `/ready`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// The model id it serves (empty if it did not say).
    pub model: String,
}

/// Ask the readiness probe: `Some` when it answers 2xx within 5 s.
pub async fn probe(http: &reqwest::Client, ready_url: &str) -> Option<Served> {
    let resp = http
        .get(ready_url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let model = resp
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v["model"].as_str().map(str::to_string))
        .unwrap_or_default();
    Some(Served { model })
}

/// Starts and stops the model process (the `mlx-air` CLI; a fake in tests).
#[async_trait]
pub trait ModelProcess: Send + Sync {
    /// Start the instance and return once the process is up (it may still be loading).
    async fn start(&self, instance: &str, model: &str, port: u16) -> anyhow::Result<()>;
    async fn stop(&self, instance: &str) -> anyhow::Result<()>;
}

/// `mlx-air serve --detach` / `mlx-air stop`. `stop` removes the instance's launchd agent, so
/// launchd's KeepAlive does not bring the model back.
pub struct MlxAir {
    pub bin: String,
    pub timeout: Duration,
}

impl MlxAir {
    async fn run(&self, args: &[&str]) -> anyhow::Result<()> {
        let out = tokio::time::timeout(
            self.timeout,
            tokio::process::Command::new(&self.bin)
                .args(args)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("{} {} timed out", self.bin, args[0]))??;
        if !out.status.success() {
            anyhow::bail!(
                "{} {} failed ({}): {}",
                self.bin,
                args[0],
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

#[async_trait]
impl ModelProcess for MlxAir {
    async fn start(&self, instance: &str, model: &str, port: u16) -> anyhow::Result<()> {
        let port = port.to_string();
        self.run(&[
            "serve", "--model", model, "--port", &port, "--detach", "--name", instance,
        ])
        .await
    }

    async fn stop(&self, instance: &str) -> anyhow::Result<()> {
        self.run(&["stop", instance]).await
    }
}

/// What the loop should do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Nothing,
    Start,
    Stop,
}

#[derive(Default)]
struct Book {
    /// The end of the last owner request (or its start while it runs).
    last_interactive: Option<DateTime<Utc>>,
    /// Since when the model is up although it should not be.
    stop_wanted_since: Option<DateTime<Utc>>,
    last_start_attempt: Option<DateTime<Utc>>,
    /// The model id the running server reported on the last probe.
    serving: Option<String>,
}

pub struct ModelControl {
    pool: PgPool,
    http: reqwest::Client,
    ready_url: String,
    /// The port of `GM_LLM_BASE_URL`; `None` when the model is not on this machine, which gm
    /// cannot start.
    port: Option<u16>,
    instance: String,
    start_timeout: Duration,
    clock: Arc<dyn Clock>,
    process: Arc<dyn ModelProcess>,
    defaults: ModelSettings,
    settings: RwLock<ModelSettings>,
    book: Mutex<Book>,
    inflight: AtomicUsize,
    /// Serializes starts and stops.
    lifecycle: tokio::sync::Mutex<()>,
}

/// Counts one model request as in flight; the idle timer restarts when an owner request ends.
pub struct RequestGuard {
    control: Arc<ModelControl>,
    interactive: bool,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.control.inflight.fetch_sub(1, Ordering::SeqCst);
        if self.interactive {
            self.control.touch();
        }
    }
}

impl ModelControl {
    pub fn new(
        cfg: &Config,
        pool: PgPool,
        http: reqwest::Client,
        clock: Arc<dyn Clock>,
        process: Option<Arc<dyn ModelProcess>>,
    ) -> Arc<Self> {
        let port = url::Url::parse(&cfg.llm_base_url)
            .ok()
            .filter(|u| {
                matches!(
                    u.host_str(),
                    Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
                )
            })
            .and_then(|u| u.port_or_known_default());
        let start_timeout = Duration::from_secs(cfg.model_start_timeout_s);
        let defaults = ModelSettings::from_config(cfg);
        Arc::new(Self {
            pool,
            http,
            ready_url: cfg.llm_ready_url.clone(),
            port,
            instance: cfg.mlx_instance.clone(),
            start_timeout,
            clock,
            process: process.unwrap_or_else(|| {
                Arc::new(MlxAir {
                    bin: cfg.mlx_air_bin.clone(),
                    timeout: start_timeout,
                })
            }),
            settings: RwLock::new(defaults.clone()),
            defaults,
            book: Mutex::new(Book::default()),
            inflight: AtomicUsize::new(0),
            lifecycle: tokio::sync::Mutex::new(()),
        })
    }

    /// Read the saved settings from Postgres (call once at startup).
    pub async fn load(&self, cfg: &Config) -> sqlx::Result<()> {
        *self.settings.write().unwrap() = ModelSettings::load(&self.pool, cfg).await?;
        Ok(())
    }

    pub fn settings(&self) -> ModelSettings {
        self.settings.read().unwrap().clone()
    }

    pub fn defaults(&self) -> &ModelSettings {
        &self.defaults
    }

    pub fn hours(&self) -> ModelHours {
        self.settings.read().unwrap().hours()
    }

    /// Whether gm controls the model process: switched on, and the model is on this machine.
    pub fn managing(&self) -> bool {
        self.port.is_some() && self.settings.read().unwrap().manage
    }

    /// Validate, store and apply new settings. Switching management off while the model is
    /// down starts it, so "off" never leaves the model stopped.
    pub async fn save(&self, new: ModelSettings) -> anyhow::Result<Result<(), SettingsError>> {
        if let Err(e) = new.validate() {
            return Ok(Err(e));
        }
        crate::db::system_set(&self.pool, SETTINGS_KEY, &serde_json::to_value(&new)?).await?;
        let was_managed = self.managing();
        *self.settings.write().unwrap() = new;
        if was_managed
            && !self.managing()
            && let Err(e) = self.start().await
        {
            tracing::warn!(error = %format!("{e:#}"), "model start after switching management off failed");
        }
        Ok(Ok(()))
    }

    /// The id the running server serves, else the configured one: what requests carry.
    pub fn request_model(&self) -> String {
        match &self.book.lock().unwrap().serving {
            Some(m) if !m.is_empty() => m.clone(),
            _ => self.settings.read().unwrap().model.clone(),
        }
    }

    fn touch(&self) {
        self.book.lock().unwrap().last_interactive = Some(self.clock_now());
    }

    fn clock_now(&self) -> DateTime<Utc> {
        self.clock.now().with_timezone(&Utc)
    }

    /// Whether the model is up now (and remember what it serves).
    pub async fn ready(&self) -> bool {
        let served = probe(&self.http, &self.ready_url).await;
        self.book.lock().unwrap().serving = served.as_ref().map(|s| s.model.clone());
        served.is_some()
    }

    /// An owner request would have to start the model first.
    pub async fn cold(&self) -> bool {
        self.managing() && !self.ready().await
    }

    /// Before a model call: an owner request (interactive lane) starts the model if it is down,
    /// and every call is counted as in flight. Background lanes never start the model; the loop
    /// does that when model hours open.
    pub async fn begin(self: &Arc<Self>, lane: Lane) -> anyhow::Result<RequestGuard> {
        let interactive = lane == Lane::Interactive;
        if interactive {
            self.touch();
            if self.managing() && !self.ready().await {
                self.start()
                    .await
                    .map_err(|e| LlmUnavailable(format!("model could not be started: {e:#}")))?;
            }
        }
        self.inflight.fetch_add(1, Ordering::SeqCst);
        Ok(RequestGuard {
            control: self.clone(),
            interactive,
        })
    }

    /// Start the model and wait until it answers `/ready`.
    async fn start(&self) -> anyhow::Result<()> {
        let _g = self.lifecycle.lock().await;
        if self.ready().await {
            return Ok(());
        }
        let Some(port) = self.port else {
            anyhow::bail!("the model is not on this machine; gm cannot start it");
        };
        let model = self.settings.read().unwrap().model.clone();
        self.book.lock().unwrap().last_start_attempt = Some(self.clock_now());
        tracing::info!(instance = %self.instance, %model, port, "starting the model");
        if let Err(e) = self.process.start(&self.instance, &model, port).await {
            // A half-started instance would make the next `serve` fail: clear it.
            let _ = self.process.stop(&self.instance).await;
            return Err(e);
        }
        let deadline = tokio::time::Instant::now() + self.start_timeout;
        loop {
            if self.ready().await {
                tracing::info!("the model is ready");
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "the model did not answer {} within {}s",
                    self.ready_url,
                    self.start_timeout.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let _g = self.lifecycle.lock().await;
        tracing::info!(instance = %self.instance, "stopping the model");
        self.process.stop(&self.instance).await
    }

    /// Whether the model should be up at `now`: inside model hours, or within `idle_min` of the
    /// last owner request.
    pub fn wanted(&self, now: DateTime<Utc>) -> bool {
        let s = self.settings.read().unwrap().clone();
        let local = now.with_timezone(&chrono::Local);
        if s.hours().is_on(local) {
            return true;
        }
        self.book
            .lock()
            .unwrap()
            .last_interactive
            .is_some_and(|t| now - t < chrono::Duration::minutes(s.idle_min as i64))
    }

    /// Decide the next step from the wanted and the real state (pure apart from the book).
    pub fn decide(&self, now: DateTime<Utc>, ready: bool) -> Step {
        if !self.managing() {
            return Step::Nothing;
        }
        let wanted = self.wanted(now);
        let mut book = self.book.lock().unwrap();
        if wanted || !ready {
            book.stop_wanted_since = None;
        }
        match (wanted, ready) {
            (true, false) => {
                if book
                    .last_start_attempt
                    .is_some_and(|t| now - t < START_RETRY)
                {
                    Step::Nothing
                } else {
                    Step::Start
                }
            }
            (false, true) => {
                let since = *book.stop_wanted_since.get_or_insert(now);
                let grace = chrono::Duration::seconds(self.settings.read().unwrap().grace_s as i64);
                if self.inflight.load(Ordering::SeqCst) == 0 || now - since >= grace {
                    Step::Stop
                } else {
                    Step::Nothing
                }
            }
            _ => Step::Nothing,
        }
    }

    /// One pass of the loop.
    pub async fn reconcile(&self) {
        let ready = self.ready().await;
        match self.decide(self.clock_now(), ready) {
            Step::Nothing => {}
            Step::Start => {
                if let Err(e) = self.start().await {
                    tracing::error!(error = %format!("{e:#}"), "starting the model failed");
                }
            }
            Step::Stop => {
                self.book.lock().unwrap().stop_wanted_since = None;
                if let Err(e) = self.stop().await {
                    tracing::error!(error = %format!("{e:#}"), "stopping the model failed");
                }
            }
        }
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            self.reconcile().await;
            tokio::time::sleep(RECONCILE_EVERY).await;
        }
    }
}
