//! Wiring: build every component from the config, run the web server and background tasks in one
//! process (ADR-0009).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::bus::Bus;
use crate::clock::{Clock, ModelHours, SystemClock};
use crate::config::Config;
use crate::crypto::SecretBox;
use crate::digest::Digest;
use crate::feedback::Feedback;
use crate::gmail::ingest::GmailIngestor;
use crate::gmail::pubsub::PubSubIngest;
use crate::google::accounts::AccountStore;
use crate::google::gmail::GmailClient;
use crate::google::pubsub::PubSubClient;
use crate::health::Health;
use crate::llm::LlmClient;
use crate::notify::{LogNotifier, Notifier, TelegramNotifier};
use crate::pipeline::Pipeline;
use crate::residency::{CommandRunner, Residency, SystemRunner};
use crate::rules::RuleFile;
use crate::scheduler::Scheduler;
use crate::telegram::TelegramPoller;
use crate::user_settings::UserSettingsStore;
use crate::web::{self, AppState, SharedState, session::Signer};

#[derive(Default)]
pub struct Overrides {
    pub notifier: Option<Arc<dyn Notifier>>,
    pub clock: Option<Arc<dyn Clock>>,
    /// Runs `launchctl` / `id` for the residency controller (a fake in tests).
    pub runner: Option<Arc<dyn CommandRunner>>,
}

pub struct App {
    pub cfg: Config,
    pub pool: PgPool,
    pub bus: Arc<Bus>,
    pub ingestor: Arc<GmailIngestor>,
    /// Present in Pub/Sub ingest mode.
    pub pubsub: Option<Arc<PubSubIngest>>,
    pub pipeline: Arc<Pipeline>,
    pub digest: Arc<Digest>,
    pub scheduler: Arc<Scheduler>,
    pub health: Arc<Health>,
    pub residency: Arc<Residency>,
    pub feedback: Arc<Feedback>,
    /// Present when Telegram is configured.
    pub telegram: Option<Arc<TelegramPoller>>,
    pub state: SharedState,
}

pub fn build(cfg: Config, pool: PgPool, overrides: Overrides) -> anyhow::Result<App> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let bus = Arc::new(Bus::new(pool.clone(), cfg.job_max_attempts));
    let accounts = AccountStore::new(
        pool.clone(),
        SecretBox::new(&cfg.master_key)?,
        http.clone(),
        &cfg,
    );
    let gmail = GmailClient::new(http.clone(), &cfg.gmail_api_base, accounts.clone());
    let ingestor = GmailIngestor::new(
        pool.clone(),
        bus.clone(),
        gmail.clone(),
        cfg.gmail_label_ids.clone(),
    );
    let pubsub = cfg.pubsub.clone().map(|ps| {
        PubSubIngest::new(
            pool.clone(),
            ingestor.clone(),
            gmail.clone(),
            PubSubClient::new(http.clone(), ps, accounts.clone()),
        )
    });
    let notifier: Arc<dyn Notifier> = match overrides.notifier {
        Some(n) => n,
        None => match (&cfg.telegram_bot_token, &cfg.telegram_chat_id) {
            (Some(token), Some(chat)) => Arc::new(TelegramNotifier::new(
                http.clone(),
                &cfg.telegram_api_base,
                token,
                chat,
            )),
            _ => {
                tracing::warn!(
                    "Telegram not configured (keychain telegram-bot-token + GM_TELEGRAM_CHAT_ID); notifications go to the log"
                );
                Arc::new(LogNotifier)
            }
        },
    };
    let user_settings = UserSettingsStore::new(
        pool.clone(),
        &cfg.default_summary_language,
        &cfg.default_ui_language,
        cfg.digest_at,
        cfg.model_hours(),
    );
    let pipeline = Arc::new(Pipeline {
        pool: pool.clone(),
        bus: bus.clone(),
        gmail: gmail.clone(),
        accounts: accounts.clone(),
        rules: Arc::new(RuleFile::new(&cfg.rules_path)),
        llm: Arc::new(LlmClient::new(&cfg, Some(pool.clone()))),
        notifier: notifier.clone(),
        user_settings: user_settings.clone(),
        clock: overrides.clock.unwrap_or_else(|| Arc::new(SystemClock)),
        hours: ModelHours {
            on: cfg.model_on,
            off: cfg.model_off,
        },
        llm_unavailable_retry: chrono::Duration::seconds(cfg.llm_unavailable_retry_s as i64),
    });
    pipeline.register();
    let health = Arc::new(Health {
        pool: pool.clone(),
        http: http.clone(),
        ready_url: cfg.llm_ready_url.clone(),
        hours: pipeline.hours,
        clock: pipeline.clock.clone(),
        notifier: notifier.clone(),
        user_settings: user_settings.clone(),
        ingestor: ingestor.clone(),
        pubsub_safety_poll_s: cfg.pubsub.as_ref().map(|p| p.safety_poll_interval_s),
        owner: cfg.allowed_email.clone(),
    });
    let feedback = Arc::new(Feedback {
        pool: pool.clone(),
        notifier: notifier.clone(),
    });
    feedback.register(&bus);
    let telegram = match (&cfg.telegram_bot_token, &cfg.telegram_chat_id) {
        (Some(token), Some(chat)) => Some(Arc::new(TelegramPoller {
            api: TelegramNotifier::new(
                reqwest::Client::builder()
                    .timeout(Duration::from_secs(crate::telegram::LONG_POLL_S + 15))
                    .build()?,
                &cfg.telegram_api_base,
                token,
                chat,
            ),
            chat_id: chat.clone(),
            pool: pool.clone(),
            bus: bus.clone(),
            notifier: notifier.clone(),
            user_settings: user_settings.clone(),
            owner: cfg.allowed_email.clone(),
        })),
        _ => None,
    };
    let digest = Arc::new(Digest {
        p: pipeline.clone(),
        problems: Some(health.clone()),
        feedback: Some(feedback.clone()),
    });
    digest.register();
    let residency = Arc::new(Residency::new(
        pool.clone(),
        http.clone(),
        &cfg.llm_ready_url,
        &cfg.mlx_launchd_label,
        overrides.runner.unwrap_or_else(|| Arc::new(SystemRunner)),
        pipeline.llm.clone(),
        pipeline.hours,
        pipeline.clock.clone(),
    ));
    residency.register(&bus);
    let scheduler = Arc::new(Scheduler {
        pool: pool.clone(),
        bus: bus.clone(),
        clock: pipeline.clock.clone(),
        hours: pipeline.hours,
        user_settings: user_settings.clone(),
        default_digest_at: cfg.digest_at,
    });
    let state = Arc::new(AppState {
        cfg: cfg.clone(),
        pool: pool.clone(),
        http,
        accounts,
        ingestor: ingestor.clone(),
        user_settings,
        signer: Signer::new(&cfg.session_secret),
        health: health.clone(),
        residency: residency.clone(),
        gmail,
        notifier_name: notifier.name(),
    });
    Ok(App {
        cfg,
        pool,
        bus,
        ingestor,
        pubsub,
        pipeline,
        digest,
        scheduler,
        health,
        residency,
        feedback,
        telegram,
        state,
    })
}

/// Keep a background loop alive: log panics/exits and restart after a pause.
fn supervise<F, Fut>(name: &'static str, factory: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match tokio::spawn(factory()).await {
                Ok(()) => tracing::warn!(task = name, "background task exited; restarting in 30s"),
                Err(e) => {
                    tracing::error!(task = name, error = %e, "background task crashed; restarting in 30s")
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

pub fn spawn_background(app: &App) {
    for i in 0..app.cfg.worker_concurrency.max(1) {
        let bus = app.bus.clone();
        supervise("bus-worker", move || bus.clone().run_worker(i));
    }
    let bus = app.bus.clone();
    supervise("requeue-stale", move || {
        let bus = bus.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(300)).await;
                if let Err(e) = bus.requeue_stale(chrono::Duration::minutes(10)).await {
                    tracing::error!(error = %e, "requeue_stale failed");
                }
            }
        }
    });
    let ingestor = app.ingestor.clone();
    let interval = Duration::from_secs(app.cfg.gmail_poll_interval_s.max(10));
    let safety = app
        .cfg
        .pubsub
        .as_ref()
        .map(|p| Duration::from_secs(p.safety_poll_interval_s.max(60)));
    supervise("gmail-poll", move || {
        ingestor.clone().poll_loop(interval, safety)
    });
    if let Some(pubsub) = app.pubsub.clone() {
        supervise("gmail-pubsub", move || pubsub.clone().run());
    }
    if let Some(poller) = app.telegram.clone() {
        supervise("telegram-updates", move || poller.clone().run());
    }
    let residency = app.residency.clone();
    supervise("residency", move || residency.clone().run());
    let health = app.health.clone();
    supervise("health", move || health.clone().run());
    let scheduler = app.scheduler.clone();
    supervise("scheduler", move || scheduler.clone().run());
    let (pool, days) = (app.pool.clone(), app.cfg.retention_days);
    supervise("retention", move || {
        let pool = pool.clone();
        async move {
            loop {
                match crate::db::apply_retention(&pool, days).await {
                    Ok((ev, jobs)) if ev + jobs > 0 => {
                        tracing::info!(events = ev, jobs, "retention cleanup")
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "retention cleanup failed"),
                }
                tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
            }
        }
    });
}

pub fn init_tracing(
    log_dir: Option<&std::path::Path>,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, fmt};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn"));
    let stderr = fmt::layer().with_target(false);
    match log_dir {
        Some(dir) => {
            let appender = tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("gm")
                .filename_suffix("log")
                .max_log_files(7)
                .build(dir)
                .expect("log directory is writable");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr)
                .with(fmt::layer().json().with_writer(writer))
                .init();
            Some(guard)
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr)
                .init();
            None
        }
    }
}

pub async fn serve(cfg: Config) -> anyhow::Result<()> {
    let pool = crate::db::connect(&cfg.database_url).await?;
    crate::db::migrate(&pool).await?;
    let app = build(cfg.clone(), pool, Overrides::default())?;
    if cfg.run_background {
        app.bus.requeue_stale(chrono::Duration::zero()).await?;
        spawn_background(&app);
    }
    let listener = tokio::net::TcpListener::bind((cfg.host.as_str(), cfg.port)).await?;
    tracing::info!(url = %cfg.base_url, "Google Management ready");
    axum::serve(listener, web::router(app.state.clone()))
        .with_graceful_shutdown(async {
            let ctrl_c = tokio::signal::ctrl_c();
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
