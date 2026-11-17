//! Health checks, alerts, dead letters and ignored senders (2E).
mod common;

use common::*;
use serde_json::json;
use sqlx::PgPool;

fn day_hours(cfg: &mut gm::config::Config) {
    cfg.model_on = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    cfg.model_off = chrono::NaiveTime::from_hms_opt(19, 0, 0).unwrap();
}

async fn dead_job(t: &TestApp) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO jobs (topic, subscriber, payload, status, attempts, last_error)
         VALUES ('notify.email', 'notify', '{}', 'dead', 5, 'boom') RETURNING id",
    )
    .fetch_one(&t.app.pool)
    .await
    .unwrap()
}

/// Ages an incident from the host clock that `Health` reads, not from Postgres' `now()`: the Docker
/// VM's clock can run a millisecond ahead, which turned "16 min" into 15 min 59.999 s.
async fn age_incident(t: &TestApp, check: &str, minutes: i64) {
    sqlx::query("UPDATE health_state SET failing_since = $2 WHERE check_name = $1")
        .bind(check)
        .bind(chrono::Utc::now() - chrono::Duration::minutes(minutes))
        .execute(&t.app.pool)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn alert_after_15_minutes_once_per_incident_then_recovery(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    mock_ready(&t.server, true).await;
    let job = dead_job(&t).await;
    let h = &t.app.health;
    h.tick().await.unwrap();
    assert!(t.sent().is_empty(), "not before 15 minutes");
    age_incident(&t, "dead_jobs", 14).await;
    h.tick().await.unwrap();
    assert!(t.sent().is_empty());
    age_incident(&t, "dead_jobs", 16).await;
    h.tick().await.unwrap();
    h.tick().await.unwrap();
    let sent = t.sent();
    assert_eq!(sent.len(), 1, "one message per incident");
    assert!(
        sent[0]
            .text
            .starts_with("🩺 Problem for 16 min: 1 dead job(s)"),
        "{}",
        sent[0].text
    );
    assert!(!sent[0].silent, "sound during the day");

    sqlx::query("UPDATE jobs SET status = 'done' WHERE id = $1")
        .bind(job)
        .execute(&t.app.pool)
        .await
        .unwrap();
    h.tick().await.unwrap();
    let sent = t.sent();
    assert_eq!(sent.len(), 2);
    assert!(
        sent[1].text.starts_with("✅ Resolved: 1 dead job(s)"),
        "{}",
        sent[1].text
    );
    assert!(h.incidents().await.unwrap().is_empty());
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn model_check_only_in_model_hours_and_alerts_are_silent_at_night(pool: PgPool) {
    let t = test_app(pool, day_hours, Some(local(23, 0))).await;
    mock_ready(&t.server, false).await;
    dead_job(&t).await;
    let results = t.app.health.evaluate().await.unwrap();
    let model = results.iter().find(|r| r.name == "model_down").unwrap();
    assert_eq!(model.failing, None, "not checked at night");
    t.app.health.tick().await.unwrap();
    age_incident(&t, "dead_jobs", 20).await;
    t.app.health.tick().await.unwrap();
    assert!(t.sent()[0].silent, "silent at night");

    let t = test_app(t.app.pool.clone(), day_hours, Some(local(10, 0))).await;
    mock_ready(&t.server, false).await;
    let results = t.app.health.evaluate().await.unwrap();
    let model = results.iter().find(|r| r.name == "model_down").unwrap();
    assert_eq!(model.failing, Some(true));
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn digest_reports_reconnect_but_not_sync_lag(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    mock_ready(&t.server, true).await;
    t.connect_owner().await;
    sqlx::query("INSERT INTO gmail_state (email, history_id, last_sync_at) VALUES ($1, 1, now() - interval '25 minutes')")
        .bind(OWNER)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.app
        .state
        .accounts
        .save(OWNER, "other@example.com", "rt", &[])
        .await
        .unwrap();
    t.app
        .state
        .accounts
        .mark_needs_reconnect("other@example.com")
        .await
        .unwrap();
    let results = t.app.health.evaluate().await.unwrap();
    let failing: Vec<_> = results
        .iter()
        .filter(|r| r.failing == Some(true))
        .map(|r| r.name)
        .collect();
    assert_eq!(failing, vec!["ingest_lag", "needs_reconnect"]);

    // No digest items, but open problems: the digest still goes out with a System section.
    t.app
        .digest
        .build(OWNER, local(9, 5).with_timezone(&chrono::Utc))
        .await
        .unwrap();
    let text = &t.sent()[0].text;
    assert!(
        text.contains("⚠️ System\n• Gmail needs to be reconnected: other@example.com"),
        "{text}"
    );
    assert!(!text.contains("not synced"), "{text}");
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn sync_lag_and_recovery_never_send_automatic_telegram_messages(pool: PgPool) {
    for hour in [10, 23] {
        let t = test_app(pool.clone(), day_hours, Some(local(hour, 0))).await;
        t.connect_owner().await;
        let stale = local(hour, 0).with_timezone(&chrono::Utc) - chrono::Duration::hours(1);
        sqlx::query(
            "INSERT INTO gmail_state (email, history_id, last_sync_at) VALUES ($1, 1, $2)
                     ON CONFLICT (email) DO UPDATE SET last_sync_at = EXCLUDED.last_sync_at",
        )
        .bind(OWNER)
        .bind(stale)
        .execute(&pool)
        .await
        .unwrap();
        t.app.health.tick().await.unwrap();
        age_incident(&t, "ingest_lag", 20).await;
        t.app.health.tick().await.unwrap();
        assert!(
            t.sent().is_empty(),
            "sync lag must not interrupt the owner at {hour}:00"
        );
        assert!(
            t.app
                .health
                .incidents()
                .await
                .unwrap()
                .iter()
                .any(|(name, _)| name == "ingest_lag")
        );

        // An incident announced by an older build must also recover without sending a message.
        sqlx::query("UPDATE health_state SET alerted_at = $1 WHERE check_name = 'ingest_lag'")
            .bind(stale)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE gmail_state SET last_sync_at = $1")
            .bind(local(hour, 0).with_timezone(&chrono::Utc))
            .execute(&pool)
            .await
            .unwrap();
        t.app.health.tick().await.unwrap();
        assert!(
            t.sent().is_empty(),
            "sync recovery must not interrupt the owner"
        );
        assert!(t.app.health.incidents().await.unwrap().is_empty());
    }
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn dead_jobs_page_retries_one_or_all(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    mock_ready(&t.server, true).await;
    let (a, b) = (dead_job(&t).await, dead_job(&t).await);
    let router = gm::web::router(t.app.state.clone());
    let cookie = session_cookie(&t, OWNER);
    let (status, _, body) = send(&router, get("/jobs", Some(&cookie))).await;
    assert_eq!(status, 200);
    assert!(body.contains("boom") && body.contains(&format!("/jobs/{a}/retry")));
    let (status, _, _) = send(&router, get("/jobs", None)).await;
    assert_eq!(status, 303, "login required");

    let (status, _, _) = send(&router, post_form(&format!("/jobs/{a}/retry"), &cookie, "")).await;
    assert_eq!(status, 303);
    let row: (String, i32, Option<String>) =
        sqlx::query_as("SELECT status, attempts, last_error FROM jobs WHERE id = $1")
            .bind(a)
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(row, ("pending".into(), 0, None));
    send(&router, post_form("/jobs/retry-dead", &cookie, "")).await;
    let dead: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE status = 'dead'")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(dead, 0, "job {b} retried too");
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn senders_page_counts_ignored_mail_by_sender(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    mock_ready(&t.server, true).await;
    for (i, (addr, rule)) in [
        ("a@shop.example", "promotions-tab"),
        ("a@shop.example", "promotions-tab"),
        ("b@x.example", "model"),
        ("c@x.example", ""),
    ]
    .iter()
    .enumerate()
    {
        let status = if rule.is_empty() { "digest" } else { "ignored" };
        sqlx::query("INSERT INTO events (account, source, kind, external_id, status, rule, data) VALUES ($1, 'gmail', 'message.received', $2, $3, $4, $5)")
            .bind(OWNER)
            .bind(format!("s{i}"))
            .bind(status)
            .bind(rule)
            .bind(json!({"from_addr": addr, "subject": format!("Sale {i}")}))
            .execute(&t.app.pool)
            .await
            .unwrap();
    }
    let router = gm::web::router(t.app.state.clone());
    let (_, _, body) = send(&router, get("/senders", Some(&session_cookie(&t, OWNER)))).await;
    let a = body.find("a@shop.example").expect("a listed");
    let b = body.find("b@x.example").expect("b listed");
    assert!(a < b, "sorted by count");
    assert!(body.contains("Sale 1") && body.contains("model"));
    assert!(!body.contains("c@x.example"), "only ignored mail");
}
