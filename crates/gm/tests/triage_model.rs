//! Model triage (2B): mail no rule covers is classified notify / digest / ignore by the model.
mod common;

use chrono::Timelike;
use common::*;
use gm::gmail::message::ParsedMessage;
use gm::llm::{LlmClient, SYSTEM_PREFIX};
use sqlx::PgPool;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

fn day_hours(cfg: &mut gm::config::Config) {
    cfg.model_on = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    cfg.model_off = chrono::NaiveTime::from_hms_opt(19, 0, 0).unwrap();
}

async fn unmatched(t: &TestApp, id: &str) -> i64 {
    t.mock_message(gmail_message(
        id,
        "Shop <info@shop.example.net>",
        "Your receipt",
        &["INBOX"],
        "Thanks for your purchase.",
    ))
    .await;
    t.insert_event(id).await
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn digest_and_ignore_decisions_are_recorded_with_reasons(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    t.connect_owner().await;
    t.mock_llm_for(TRIAGE, &triage_json("digest", "a receipt"))
        .await;
    let a = unmatched(&t, "d1").await;
    t.drain().await;
    assert_eq!(t.event_status(a).await, "digest");
    let ev = gm::db::event(&t.app.pool, a).await.unwrap().unwrap();
    assert_eq!(ev.rule.as_deref(), Some("model"));
    assert_eq!(ev.data["verdict"]["section"], "model");
    assert_eq!(ev.data["verdict"]["reason"], "a receipt");
    assert_eq!(ev.data["verdict"]["summarize"], true);
    assert!(t.sent().is_empty());

    t.server.reset().await;
    t.connect_owner().await;
    t.mock_llm_for(TRIAGE, &triage_json("ignore", "marketing"))
        .await;
    let b = unmatched(&t, "d2").await;
    t.drain().await;
    assert_eq!(t.event_status(b).await, "ignored");
    let ev = gm::db::event(&t.app.pool, b).await.unwrap().unwrap();
    assert_eq!(ev.data["verdict"]["summarize"], false);
    assert!(t.sent().is_empty());
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn unusable_output_fails_open_to_notify(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    t.connect_owner().await;
    t.mock_llm_for(TRIAGE, "I think you should archive it.")
        .await;
    t.mock_llm(SUMMARY_JSON).await;
    let id = unmatched(&t, "d3").await;
    t.drain().await;
    assert_eq!(t.event_status(id).await, "notified");
    assert_eq!(t.sent().len(), 1);
    let retries: i64 =
        sqlx::query_scalar("SELECT count(*) FROM llm_calls WHERE task = 'triage-retry'")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(retries, 1, "one format-reminder retry before failing open");
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn night_and_outage_defer_triage(pool: PgPool) {
    let t = test_app(pool, day_hours, Some(local(21, 0))).await;
    t.connect_owner().await;
    let id = unmatched(&t, "d4").await;
    t.drain().await;
    assert_eq!(t.event_status(id).await, "classify");
    let (attempts, run_after): (i32, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT attempts, run_after FROM jobs WHERE subscriber = 'classify'")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(attempts, 0);
    let l = run_after.with_timezone(&chrono::Local);
    assert_eq!((l.hour(), l.minute()), (9, 0));
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn unavailable_model_queues_triage(pool: PgPool) {
    let t = test_app(pool, |c| c.job_max_attempts = 1, None).await;
    t.connect_owner().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&t.server)
        .await;
    let id = unmatched(&t, "d5").await;
    t.drain().await;
    assert_eq!(t.event_status(id).await, "classify");
    let (status, attempts): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM jobs WHERE subscriber = 'classify'")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), attempts), ("pending", 0));
}

#[test]
fn triage_prompt_keeps_the_shared_prefix() {
    let llm = LlmClient::new(&gm::config::Config::for_tests(), None);
    let m = ParsedMessage {
        subject: "Hello".into(),
        body_text: "x ".repeat(10_000),
        ..Default::default()
    };
    let triage = gm::triage_model::messages(&llm, &m, "English");
    let summary = llm.summary_messages(&m, "English", &[]);
    let (ts, ss) = (
        triage[0]["content"].as_str().unwrap(),
        summary[0]["content"].as_str().unwrap(),
    );
    assert!(ts.starts_with(SYSTEM_PREFIX.trim_end()));
    assert!(ss.starts_with(SYSTEM_PREFIX.trim_end()));
    assert!(ts.contains("Task: decide how") && ts.ends_with("/no_think"));
    let user = triage[1]["content"].as_str().unwrap();
    assert!(user.starts_with("Output language: English"));
    assert!(user.ends_with("</email>"));
    assert!(
        user.contains("[...truncated...]"),
        "body capped at 1,500 tokens"
    );
    assert!(user.len() < summary[1]["content"].as_str().unwrap().len());
}
