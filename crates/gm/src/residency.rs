//! Model residency controller (workstream 2D, ADR-0010, decisions L3, N10, N11).
//!
//! When the machine-wide switch `model_residency` is on (dashboard; off by default), gm starts the
//! mlx-server-runtime launchd agent at `GM_MODEL_ON` (`launchctl kickstart`) and waits for `/ready`,
//! and stops it at `GM_MODEL_OFF` (`launchctl kill TERM`) after in-flight model requests finish (at
//! most 5 minutes). At startup, when the switch is turned on, and every 10 minutes it reconciles:
//! inside model hours the model should be up, outside down. While the switch is off gm never runs
//! `launchctl`. `ModelHours` stays the single source of truth: the pipeline defers model work
//! outside them regardless.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::OnceCell;

use crate::bus::{JobContext, Outcome};
use crate::clock::{Clock, ModelHours};
use crate::digest::ScheduleDue;
use crate::health::model_ready;
use crate::llm::LlmClient;

pub const SETTING: &str = "model_residency";

/// Runs an external program (a fake in tests). Returns stdout; non-zero exit is an error.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String>;
}

pub struct SystemRunner;

#[async_trait]
impl CommandRunner for SystemRunner {
    async fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String> {
        let out = tokio::process::Command::new(program)
            .args(args)
            .output()
            .await?;
        if !out.status.success() {
            let err: String = String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(300)
                .collect();
            anyhow::bail!(
                "{program} {} failed ({}): {err}",
                args.join(" "),
                out.status
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

pub struct Residency {
    pub pool: PgPool,
    pub http: reqwest::Client,
    pub ready_url: String,
    pub label: String,
    pub runner: Arc<dyn CommandRunner>,
    pub llm: Arc<LlmClient>,
    pub hours: ModelHours,
    pub clock: Arc<dyn Clock>,
    /// How long to wait for `/ready` after a start (then the health check takes over).
    pub load_timeout: Duration,
    /// How long to wait for in-flight requests before stopping (decision N11: 5 minutes).
    pub unload_wait: Duration,
    /// Poll interval for `/ready` and the in-flight counter.
    pub poll: Duration,
    uid: OnceCell<String>,
}

impl Residency {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        http: reqwest::Client,
        ready_url: &str,
        label: &str,
        runner: Arc<dyn CommandRunner>,
        llm: Arc<LlmClient>,
        hours: ModelHours,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            pool,
            http,
            ready_url: ready_url.into(),
            label: label.into(),
            runner,
            llm,
            hours,
            clock,
            load_timeout: Duration::from_secs(300),
            unload_wait: Duration::from_secs(300),
            poll: Duration::from_secs(2),
            uid: OnceCell::new(),
        }
    }

    pub async fn enabled(&self) -> anyhow::Result<bool> {
        Ok(crate::db::system_get(&self.pool, SETTING)
            .await?
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    /// Switch residency on or off (dashboard). Turning it on applies the current state now.
    pub async fn set_enabled(&self, on: bool) -> anyhow::Result<()> {
        crate::db::system_set(&self.pool, SETTING, &json!(on)).await?;
        tracing::info!(on, "model residency switched");
        if on {
            self.reconcile().await?;
        }
        Ok(())
    }

    pub async fn resident(&self) -> bool {
        model_ready(&self.http, &self.ready_url).await
    }

    async fn target(&self) -> anyhow::Result<String> {
        let uid = self
            .uid
            .get_or_try_init(|| async {
                let out = self.runner.run("id", &["-u".to_string()]).await?;
                anyhow::Ok(out.trim().to_string())
            })
            .await?;
        Ok(format!("gui/{uid}/{}", self.label))
    }

    /// Start the agent and wait for `/ready`. Returns whether it became ready in time; a timeout is
    /// logged and left to the health check ("model down" alert after 15 minutes).
    pub async fn load(&self) -> anyhow::Result<bool> {
        let target = self.target().await?;
        self.runner
            .run("launchctl", &["kickstart".into(), target.clone()])
            .await?;
        let deadline = tokio::time::Instant::now() + self.load_timeout;
        loop {
            if self.resident().await {
                tracing::info!(%target, "model loaded");
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::error!(%target, timeout_s = self.load_timeout.as_secs(), "model did not become ready");
                return Ok(false);
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    /// Wait for in-flight model requests (at most `unload_wait`), then stop the agent.
    pub async fn unload(&self) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + self.unload_wait;
        while self.llm.inflight() > 0 {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    inflight = self.llm.inflight(),
                    "stopping the model with requests in flight"
                );
                break;
            }
            tokio::time::sleep(self.poll).await;
        }
        let target = self.target().await?;
        // Not running is fine: nothing to stop.
        if let Err(e) = self
            .runner
            .run("launchctl", &["kill".into(), "TERM".into(), target.clone()])
            .await
        {
            tracing::warn!(%target, error = %e, "launchctl kill failed (agent not running?)");
        } else {
            tracing::info!(%target, "model unloaded");
        }
        Ok(())
    }

    /// Inside model hours the model should be up, outside down. No-op while the switch is off.
    pub async fn reconcile(&self) -> anyhow::Result<()> {
        if !self.enabled().await? {
            return Ok(());
        }
        let up = self.resident().await;
        if self.hours.is_on(self.clock.now()) {
            if !up {
                self.load().await?;
            }
        } else if up {
            self.unload().await?;
        }
        Ok(())
    }

    pub async fn on_schedule(&self, payload: Value, _ctx: JobContext) -> anyhow::Result<Outcome> {
        let due: ScheduleDue = serde_json::from_value(payload)?;
        if !matches!(due.kind.as_str(), "load" | "unload") || !self.enabled().await? {
            return Ok(Outcome::Done);
        }
        if due.kind == "load" {
            self.load().await?;
        } else {
            self.unload().await?;
        }
        Ok(Outcome::Done)
    }

    pub fn register(self: &Arc<Self>, bus: &crate::bus::Bus) {
        let r = self.clone();
        bus.subscribe(crate::topics::SCHEDULE_DUE, "residency", move |v, c| {
            let r = r.clone();
            async move { r.on_schedule(v, c).await }
        });
    }

    /// Reconcile at startup and every 10 minutes (recovers from crashes and missed schedules).
    pub async fn run(self: Arc<Self>) {
        loop {
            if let Err(e) = self.reconcile().await {
                tracing::error!(error = %e, "residency reconcile failed");
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    }
}
