//! Residency controller (2D): fake launchctl, wiremock /ready.
mod common;

use std::time::Duration;

use common::*;
use gm::llm::Lane;
use serde_json::json;
use sqlx::PgPool;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

fn day_hours(cfg: &mut gm::config::Config) {
    cfg.model_on = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    cfg.model_off = chrono::NaiveTime::from_hms_opt(19, 0, 0).unwrap();
}

async fn due(t: &TestApp, kind: &str) {
    let mut tx = t.app.pool.begin().await.unwrap();
    t.app
        .bus
        .publish(
            &mut tx,
            gm::topics::SCHEDULE_DUE,
            &json!({"kind": kind, "at": chrono::Utc::now()}),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    t.drain().await;
}

async fn enable(t: &TestApp) {
    gm::db::system_set(&t.app.pool, gm::residency::SETTING, &json!(true))
        .await
        .unwrap();
}

fn fast(r: &gm::residency::Residency) -> gm::residency::Residency {
    let mut r2 = gm::residency::Residency::new(
        r.pool.clone(),
        r.http.clone(),
        &r.ready_url,
        &r.label,
        r.runner.clone(),
        r.llm.clone(),
        r.hours,
        r.clock.clone(),
    );
    r2.poll = Duration::from_millis(20);
    r2.load_timeout = Duration::from_millis(300);
    r2
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn off_by_default_never_runs_launchctl(pool: PgPool) {
    let t = test_app(pool, day_hours, Some(local(9, 0))).await;
    assert!(!t.app.residency.enabled().await.unwrap());
    due(&t, "load").await;
    due(&t, "unload").await;
    t.app.residency.reconcile().await.unwrap();
    assert!(t.runner.calls.lock().unwrap().is_empty());
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn load_kickstarts_and_waits_for_ready(pool: PgPool) {
    let t = test_app(pool, day_hours, Some(local(9, 0))).await;
    enable(&t).await;
    Mock::given(method("GET"))
        .and(path("/ready"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&t.server)
        .await;
    let r = fast(&t.app.residency);
    assert!(r.load().await.unwrap(), "ready after two 503s");
    assert_eq!(
        t.runner.launchctl(),
        vec!["launchctl kickstart gui/501/com.gm.mlx"]
    );

    // Never ready: gives up after the timeout (the health check alerts later).
    mock_ready(&t.server, false).await;
    assert!(!r.load().await.unwrap());
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn unload_waits_for_in_flight_requests(pool: PgPool) {
    let t = test_app(pool, day_hours, Some(local(19, 0))).await;
    enable(&t).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(400))
                .set_body_json(json!({"choices": [{"message": {"content": "{}"}}]})),
        )
        .mount(&t.server)
        .await;
    let llm = t.app.pipeline.llm.clone();
    let req = tokio::spawn(async move {
        llm.chat(
            "summary",
            Lane::Notify,
            &[json!({"role": "user", "content": "x"})],
            10,
            0.0,
        )
        .await
        .unwrap();
        std::time::Instant::now()
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(t.app.pipeline.llm.inflight(), 1);
    let r = fast(&t.app.residency);
    r.unload().await.unwrap();
    let killed = std::time::Instant::now();
    let finished = req.await.unwrap();
    assert!(finished <= killed, "killed only after the request finished");
    assert_eq!(
        t.runner.launchctl(),
        vec!["launchctl kill TERM gui/501/com.gm.mlx"]
    );
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn unload_gives_up_waiting_after_the_limit(pool: PgPool) {
    let t = test_app(pool, day_hours, Some(local(19, 0))).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .mount(&t.server)
        .await;
    let llm = t.app.pipeline.llm.clone();
    tokio::spawn(async move {
        let _ = llm
            .chat(
                "summary",
                Lane::Notify,
                &[json!({"role": "user", "content": "x"})],
                10,
                0.0,
            )
            .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut r = fast(&t.app.residency);
    r.unload_wait = Duration::from_millis(200);
    let started = std::time::Instant::now();
    r.unload().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        t.app.pipeline.llm.inflight(),
        1,
        "stopped with the request in flight"
    );
    assert_eq!(t.runner.launchctl().len(), 1);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn reconcile_and_schedule_follow_model_hours(pool: PgPool) {
    // Night, model still up: stopped.
    let t = test_app(pool, day_hours, Some(local(22, 0))).await;
    enable(&t).await;
    t.app.residency.reconcile().await.unwrap();
    assert_eq!(
        t.runner.launchctl(),
        vec!["launchctl kill TERM gui/501/com.gm.mlx"]
    );

    // Day, model down: started; schedule.due{load} also starts it.
    let t = test_app(t.app.pool.clone(), day_hours, Some(local(10, 0))).await;
    mock_ready(&t.server, false).await;
    let r = fast(&t.app.residency);
    r.reconcile().await.unwrap();
    assert_eq!(
        t.runner.launchctl(),
        vec!["launchctl kickstart gui/501/com.gm.mlx"]
    );
    due(&t, "unload").await;
    assert_eq!(
        t.runner.launchctl().last().unwrap(),
        "launchctl kill TERM gui/501/com.gm.mlx"
    );
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn dashboard_switch_and_metric(pool: PgPool) {
    let t = test_app(pool, |c| c.metrics_token = Some("m".into()), None).await;
    let router = gm::web::router(t.app.state.clone());
    let cookie = session_cookie(&t, OWNER);
    let (_, _, body) = send(&router, get("/", Some(&cookie))).await;
    assert!(body.contains("Turn on"), "off by default");
    let (status, _, _) = send(&router, post_form("/model/residency", &cookie, "on=1")).await;
    assert_eq!(status, 303);
    assert!(t.app.residency.enabled().await.unwrap());
    let (_, _, body) = send(&router, get("/", Some(&cookie))).await;
    assert!(body.contains("Turn off"));
    send(&router, post_form("/model/residency", &cookie, "on=0")).await;
    assert!(!t.app.residency.enabled().await.unwrap());

    let req = axum::http::Request::get("/metrics")
        .header("host", "localhost:8080")
        .header("authorization", "Bearer m")
        .body(axum::body::Body::empty())
        .unwrap();
    let (_, _, body) = send(&router, req).await;
    assert!(body.contains("gm_model_resident 1"), "{body}");
}
