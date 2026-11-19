//! Model residency (ADR-0010): gm stops the model after hours, starts it for owner requests,
//! and leaves it alone when management is off. The `mlx-air` CLI is a fake.

mod common;

use chrono::{Duration, Utc};
use common::*;
use gm::llm::Lane;
use gm::model::{ModelSettings, SettingsError, Step};
use sqlx::PgPool;

async fn setup(
    pool: PgPool,
    now: chrono::DateTime<chrono::Local>,
    up: bool,
) -> (TestApp, std::sync::Arc<FakeModel>) {
    let fake = FakeModel::new(up);
    let t = test_app_with(pool, managed_day_hours, Some(now), Some(fake.clone())).await;
    fake.mount_ready(&t.server).await;
    (t, fake)
}

#[sqlx::test(migrations = "./migrations")]
async fn the_model_is_stopped_after_hours_and_started_when_hours_open(pool: PgPool) {
    let (t, fake) = setup(pool.clone(), local(23, 0), true).await;
    t.app.model.reconcile().await;
    assert_eq!(
        fake.calls(),
        vec!["stop gm"],
        "up at 23:00 with nobody asking"
    );
    t.app.model.reconcile().await;
    assert_eq!(fake.calls().len(), 1, "already stopped: nothing more");

    let (t, fake) = setup(pool, local(10, 0), false).await;
    t.app.model.reconcile().await;
    let port = t.server.address().port();
    assert_eq!(
        fake.calls(),
        vec![format!("start gm test-model {port}")],
        "down at 10:00: started with the model id and the port of GM_LLM_BASE_URL"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_owner_request_at_night_starts_the_model_and_keeps_it_for_the_idle_time(pool: PgPool) {
    let (t, fake) = setup(pool, local(23, 0), false).await;
    t.mock_llm(r#"{"ok": true}"#).await;
    assert!(t.app.model.cold().await);
    let llm = &t.app.tasks.llm;
    llm.chat("test", Lane::Interactive, &[], 10, 0.0)
        .await
        .unwrap();
    assert!(fake.is_up());
    assert_eq!(fake.calls().len(), 1);

    // The request ended just now (fixed clock): within 10 idle minutes the model stays.
    let fixed = local(23, 0).with_timezone(&Utc);
    assert_eq!(
        t.app.model.decide(fixed + Duration::minutes(5), true),
        Step::Nothing
    );
    assert_eq!(
        t.app.model.decide(fixed + Duration::minutes(11), true),
        Step::Stop
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn background_work_never_starts_the_model_at_night(pool: PgPool) {
    let (t, fake) = setup(pool, local(23, 0), false).await;
    t.mock_llm("{}").await;
    // The batch lane goes straight to the server; it never starts the model first.
    t.app
        .tasks
        .llm
        .chat("test", Lane::Batch, &[], 10, 0.0)
        .await
        .unwrap();
    assert!(fake.calls().is_empty(), "no start for a background call");
    assert_eq!(
        t.app.model.decide(local(23, 0).with_timezone(&Utc), false),
        Step::Nothing
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn stopping_waits_for_requests_in_flight_up_to_the_grace(pool: PgPool) {
    let (t, _fake) = setup(pool, local(18, 59), true).await;
    let guard = t.app.model.begin(Lane::Batch).await.unwrap();
    let t0 = local(19, 0).with_timezone(&Utc);
    assert_eq!(
        t.app.model.decide(t0, true),
        Step::Nothing,
        "a request is running"
    );
    assert_eq!(
        t.app.model.decide(t0 + Duration::seconds(299), true),
        Step::Nothing
    );
    assert_eq!(
        t.app.model.decide(t0 + Duration::seconds(301), true),
        Step::Stop,
        "grace is 300 s"
    );
    drop(guard);
    assert_eq!(
        t.app.model.decide(t0 + Duration::seconds(302), true),
        Step::Stop,
        "idle: stop now"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn with_management_off_the_model_is_left_alone_and_started_when_switched_off(pool: PgPool) {
    let (t, fake) = setup(pool, local(23, 0), true).await;
    let mut s = t.app.model.settings();
    s.manage = false;
    assert!(t.app.model.save(s.clone()).await.unwrap().is_ok());
    assert_eq!(
        t.app.model.decide(local(23, 0).with_timezone(&Utc), true),
        Step::Nothing
    );
    assert!(
        fake.calls().is_empty(),
        "up already: switching off starts nothing"
    );

    // Down, management on → off: the model is started so "off" never leaves it stopped.
    s.manage = true;
    t.app.model.save(s.clone()).await.unwrap().unwrap();
    fake.up.store(false, std::sync::atomic::Ordering::SeqCst);
    s.manage = false;
    t.app.model.save(s).await.unwrap().unwrap();
    assert!(fake.is_up());
}

#[sqlx::test(migrations = "./migrations")]
async fn settings_are_validated_stored_and_loaded(pool: PgPool) {
    let (t, _fake) = setup(pool.clone(), local(10, 0), true).await;
    let mut s = t.app.model.settings();
    s.idle_min = 0;
    assert_eq!(
        t.app.model.save(s.clone()).await.unwrap(),
        Err(SettingsError::IdleMinutes)
    );
    s.idle_min = 30;
    s.model = "has space".into();
    assert_eq!(
        t.app.model.save(s.clone()).await.unwrap(),
        Err(SettingsError::Model)
    );
    s.model = "other/model".into();
    s.grace_s = 60;
    t.app.model.save(s.clone()).await.unwrap().unwrap();
    let loaded = ModelSettings::load(&pool, &t.app.cfg).await.unwrap();
    assert_eq!(loaded, s, "the dashboard value wins over .env");
}

#[sqlx::test(migrations = "./migrations")]
async fn telegram_rest_survives_restart_and_blocks_model_work_until_wakeup(pool: PgPool) {
    let configure = |cfg: &mut gm::config::Config| {
        telegram(cfg);
        managed_day_hours(cfg);
    };
    let fake = FakeModel::new(true);
    let t = test_app_with(
        pool.clone(),
        configure,
        Some(local(10, 0)),
        Some(fake.clone()),
    )
    .await;
    fake.mount_ready(&t.server).await;

    t.update(text_update(1, 99, "/rest", None)).await;
    assert!(fake.is_up());
    assert!(t.sent().is_empty());
    t.update(text_update(2, 42, "/rest@MyBot", None)).await;
    assert!(!fake.is_up());
    assert_eq!(
        t.last_sent().text,
        "The model is off. Send /wakeup to start it again."
    );
    drop(t);

    let t = test_app_with(pool, configure, Some(local(10, 0)), Some(fake.clone())).await;
    fake.mount_ready(&t.server).await;
    t.app.model.load(&t.app.cfg).await.unwrap();
    t.app.model.reconcile().await;
    assert!(
        !fake.is_up(),
        "the daytime scheduler honors rest after restart"
    );
    for lane in [Lane::Batch, Lane::Interactive] {
        let error = t
            .app
            .tasks
            .llm
            .chat("test", lane, &[], 10, 0.0)
            .await
            .unwrap_err();
        assert!(
            gm::llm::LlmUnavailable::is(&error),
            "model jobs defer while resting"
        );
    }
    assert_eq!(requests_to(&t.server, "POST", "/chat/completions").await, 0);

    t.update(text_update(3, 42, "/status", None)).await;
    let status = t.last_sent().text;
    assert!(status.contains("rest until /wakeup"));
    assert!(status.contains("No problems."));
    t.update(text_update(4, 42, "/wakeup", None)).await;
    assert_eq!(t.last_sent().text, "The model is ready.");
    assert!(fake.is_up());
    t.app.model.load(&t.app.cfg).await.unwrap();
    t.mock_llm("ready again").await;
    assert_eq!(
        t.app
            .tasks
            .llm
            .chat("test", Lane::Interactive, &[], 10, 0.0)
            .await
            .unwrap(),
        "ready again"
    );
}
