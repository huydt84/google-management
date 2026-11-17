//! Morning digest and scheduler (2A).
mod common;

use std::sync::Arc;

use chrono::{Timelike, Utc};
use common::*;
use gm::clock::FixedClock;
use gm::scheduler::Scheduler;
use serde_json::{Value, json};
use sqlx::PgPool;

fn day_hours(cfg: &mut gm::config::Config) {
    cfg.model_on = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    cfg.model_off = chrono::NaiveTime::from_hms_opt(19, 0, 0).unwrap();
}

/// A digest event as triage would leave it.
async fn digest_event(
    t: &TestApp,
    ext: &str,
    from_name: &str,
    subject: &str,
    verdict: Value,
    summary: Option<Value>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO events (account, source, kind, external_id, status, rule, data, summary)
         VALUES ($1, 'gmail', 'message.received', $2, 'digest', $3, $4, $5) RETURNING id",
    )
    .bind(OWNER)
    .bind(ext)
    .bind(verdict["rule"].as_str().unwrap())
    .bind(json!({"from_name": from_name, "from": format!("{from_name} <x@example.com>"), "subject": subject, "verdict": verdict}))
    .bind(summary)
    .fetch_one(&t.app.pool)
    .await
    .unwrap()
}

fn v(rule: &str, section: &str, summarize: bool) -> Value {
    json!({"rule": rule, "action": "digest", "priority": "normal", "summarize": summarize,
           "section": section, "message": null, "extracted": {}})
}

fn at_0905() -> chrono::DateTime<Utc> {
    local(9, 5).with_timezone(&Utc)
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn sections_follow_rule_order_jobs_are_deduped_and_model_items_come_last(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    t.connect_owner().await;
    let mut model = v("model", "model", true);
    model["reason"] = json!("a receipt");
    digest_event(
        &t,
        "e1",
        "Shop",
        "Receipt",
        model,
        Some(json!({"line": "You paid 1,000 yen."})),
    )
    .await;
    digest_event(
        &t,
        "e2",
        "Coursera",
        "Weekly",
        v("newsletters", "newsletters", true),
        Some(json!({"line": "New courses."})),
    )
    .await;
    let job = |t: &str, c: &str| json!({"title": t, "company": c, "location": "Tokyo", "link": ""});
    digest_event(
        &t,
        "e3",
        "LinkedIn",
        "jobs 1",
        v("linkedin-job-alerts", "jobs", true),
        Some(json!({"jobs": [job("Engineer", "Acme"), job("Designer", "Beta")]})),
    )
    .await;
    digest_event(
        &t,
        "e4",
        "LinkedIn",
        "jobs 2",
        v("linkedin-job-alerts", "jobs", true),
        Some(json!({"jobs": [job("engineer", "ACME"), job("SRE", "Gamma")]})),
    )
    .await;
    digest_event(
        &t,
        "e5",
        "楽天カード",
        "カード利用のお知らせ",
        v("rakuten-card-confirmed", "card-usage", false),
        None,
    )
    .await;
    digest_event(
        &t,
        "e6",
        "Orders",
        "注文確認",
        v("rakuten-order-confirmation", "orders", true),
        None,
    )
    .await;

    t.app.digest.build(OWNER, at_0905()).await.unwrap();
    let sent = t.sent();
    assert_eq!(sent.len(), 1);
    let text = &sent[0].text;
    assert!(sent[0].silent, "digest is silent (N7)");
    let pos = |s: &str| {
        text.find(s)
            .unwrap_or_else(|| panic!("{s:?} missing in:\n{text}"))
    };
    assert!(pos("💳 Card usage") < pos("📦 Orders"));
    assert!(pos("📦 Orders") < pos("💼 Job alerts"));
    assert!(pos("💼 Job alerts") < pos("📰 Newsletters"));
    assert!(pos("📰 Newsletters") < pos("📨 Other mail"));
    assert_eq!(
        text.matches("Engineer").count() + text.matches("engineer").count(),
        1,
        "deduped"
    );
    assert!(text.contains("• SRE — Gamma · Tokyo"));
    assert!(
        text.contains("• 楽天カード: カード利用のお知らせ"),
        "rule-only item by subject"
    );
    assert!(
        text.contains("• ⏳ Orders: 注文確認"),
        "pending item marked"
    );
    assert!(text.contains("[1] Shop: You paid 1,000 yen.\n    ↳ a receipt"));
    assert!(text.contains("⏳ Summary still being written"));
    assert_eq!(sent[0].buttons.len(), 1, "one feedback row per model item");
    assert_eq!(sent[0].buttons[0][1].text, "👎 1");

    let (status, items, ids): (String, i32, Value) =
        sqlx::query_as("SELECT status, item_count, message_ids FROM digests")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), items), ("sent", 6));
    assert_eq!(ids, json!([1]));

    // Retried or re-fired the same day: nothing is sent twice.
    t.app.digest.build(OWNER, at_0905()).await.unwrap();
    assert_eq!(t.sent().len(), 1);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn empty_digest_sends_nothing_and_items_are_never_repeated(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    t.connect_owner().await;
    t.app.digest.build(OWNER, at_0905()).await.unwrap();
    assert!(t.sent().is_empty(), "N2: nothing new, nothing sent");
    let status: String = sqlx::query_scalar("SELECT status FROM digests")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(status, "empty");

    // An item that arrives afterwards goes into the next day's digest only.
    digest_event(
        &t,
        "n1",
        "Coursera",
        "Weekly",
        v("newsletters", "newsletters", true),
        Some(json!({"line": "x"})),
    )
    .await;
    let tomorrow = at_0905() + chrono::Duration::days(1);
    t.app.digest.build(OWNER, tomorrow).await.unwrap();
    t.app
        .digest
        .build(OWNER, tomorrow + chrono::Duration::days(1))
        .await
        .unwrap();
    assert_eq!(t.sent().len(), 1);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn first_digest_covers_only_the_last_24_hours(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    t.connect_owner().await;
    let old = digest_event(
        &t,
        "o1",
        "Old",
        "Old news",
        v("newsletters", "newsletters", false),
        None,
    )
    .await;
    sqlx::query("UPDATE events SET created_at = now() - interval '3 days' WHERE id = $1")
        .bind(old)
        .execute(&t.app.pool)
        .await
        .unwrap();
    digest_event(
        &t,
        "o2",
        "New",
        "Fresh news",
        v("newsletters", "newsletters", false),
        None,
    )
    .await;
    t.app.digest.build(OWNER, at_0905()).await.unwrap();
    let text = &t.sent()[0].text;
    assert!(
        text.contains("Fresh news") && !text.contains("Old news"),
        "{text}"
    );
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn digest_item_summaries_wait_for_model_hours(pool: PgPool) {
    let t = test_app(pool, day_hours, Some(local(23, 0))).await;
    t.connect_owner().await;
    t.mock_message(gmail_message(
        "nl1",
        "Coursera <Coursera@m.learn.coursera.org>",
        "Recommended for you",
        &["INBOX"],
        "Three new courses.",
    ))
    .await;
    let id = t.insert_event("nl1").await;
    t.drain().await;
    assert_eq!(t.event_status(id).await, "digest");
    let run_after: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT run_after FROM jobs WHERE subscriber = 'digest_summarize'")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    let l = run_after.with_timezone(&chrono::Local);
    assert_eq!(
        (l.hour(), l.minute()),
        (9, 0),
        "overnight backlog runs at 09:00 (L10)"
    );
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn late_summary_edits_the_sent_digest(pool: PgPool) {
    let t = test_app(pool, day_hours, Some(local(10, 0))).await;
    t.connect_owner().await;
    t.mock_message(gmail_message(
        "late",
        "Coursera <Coursera@m.learn.coursera.org>",
        "Recommended for you",
        &["INBOX"],
        "Three new courses.",
    ))
    .await;
    t.mock_llm(r#"{"line": "Three new data courses."}"#).await;
    let id = digest_event(
        &t,
        "late",
        "Coursera",
        "Recommended for you",
        v("newsletters", "newsletters", true),
        None,
    )
    .await;
    t.app.digest.build(OWNER, at_0905()).await.unwrap();
    assert!(
        t.sent()[0]
            .text
            .contains("⏳ Coursera: Recommended for you")
    );

    let mut tx = t.app.pool.begin().await.unwrap();
    t.app
        .bus
        .publish(
            &mut tx,
            gm::topics::DIGEST_ITEM_SUMMARIZE,
            &json!({"event_id": id}),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    t.drain().await;
    let edits = t.notifier.edits.lock().unwrap().clone();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].0, 1, "edits the message sent at 09:05");
    assert!(
        edits[0]
            .1
            .text
            .contains("• Coursera: Three new data courses.")
    );
    assert!(!edits[0].1.text.contains('⏳'));
    assert_eq!(t.sent().len(), 1, "edited, not resent");
    let lane: String = sqlx::query_scalar("SELECT lane FROM llm_calls LIMIT 1")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(lane, "batch");
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn scheduler_fires_missed_times_once(pool: PgPool) {
    let t = test_app(pool, day_hours, None).await;
    t.connect_owner().await;
    let sched = |now| Scheduler {
        pool: t.app.pool.clone(),
        bus: t.app.bus.clone(),
        clock: Arc::new(FixedClock(now)),
        user_settings: t.app.state.user_settings.clone(),
        default_digest_at: chrono::NaiveTime::from_hms_opt(9, 5, 0).unwrap(),
    };
    let due = |pool: PgPool| async move {
        sqlx::query_scalar::<_, Value>(
            "SELECT payload FROM jobs WHERE topic = 'schedule.due' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    assert_eq!(
        sched(local(8, 0)).tick().await.unwrap(),
        0,
        "first sight: record only"
    );
    // Mac asleep from 08:00 until 14:00: the 09:05 digest fires once on wake.
    assert_eq!(sched(local(14, 0)).tick().await.unwrap(), 1);
    assert_eq!(sched(local(14, 1)).tick().await.unwrap(), 0, "never twice");
    let jobs = due(t.app.pool.clone()).await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["kind"], "digest");
    // The digest job builds the digest for that owner and date.
    t.drain().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM digests")
            .fetch_one(&t.app.pool)
            .await
            .unwrap(),
        1
    );
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn digest_time_must_fall_inside_model_hours(pool: PgPool) {
    let t = test_app(pool, day_hours, None).await;
    let store = &t.app.state.user_settings;
    let mut s = store.get(OWNER).await.unwrap();
    assert_eq!(s.digest_at, "09:05", "GM_DIGEST_AT default");
    s.digest_at = "08:30".into();
    assert_eq!(
        store.save(OWNER, s.clone()).await.unwrap(),
        Err(gm::user_settings::SettingsError::DigestTime)
    );
    s.digest_at = "7:5".into();
    assert!(store.save(OWNER, s.clone()).await.unwrap().is_err());
    s.digest_at = "12:30".into();
    assert!(store.save(OWNER, s).await.unwrap().is_ok());
    assert_eq!(store.get(OWNER).await.unwrap().digest_at, "12:30");
}
