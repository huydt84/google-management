//! Summaries while the model is unavailable or failing (decision L12): queued, never a ping
//! without a summary. Kept apart from pipeline.rs because the sqlx 0.8 test harness stalls for 30 s
//! at cleanup when one test binary runs 10 or more `#[sqlx::test]`s (each leaks two of its 20
//! master-pool permits); keep every test file below 10 tests.
mod common;

use common::*;
use sqlx::PgPool;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn unavailable_model_queues_the_summary_without_using_attempts(pool: PgPool) {
    let t = test_app(pool, |c| c.job_max_attempts = 1, None).await;
    t.connect_owner().await;
    t.mock_message(gmail_message(
        "m8",
        "Carol <carol@example.org>",
        "Hello",
        &["INBOX"],
        "Hi",
    ))
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&t.server)
        .await;
    let id = t.insert_event("m8").await;
    t.drain().await;
    assert_eq!(t.event_status(id).await, "triaged");
    assert!(t.sent().is_empty(), "no ping without a summary");
    let (status, attempts, run_after): (String, i32, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as(
            "SELECT status, attempts, run_after FROM jobs WHERE subscriber = 'summarize'",
        )
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!((status.as_str(), attempts), ("pending", 0));
    let wait = (run_after - chrono::Utc::now()).num_seconds();
    assert!(
        (280..=300).contains(&wait),
        "retries in 5 minutes, got {wait}s"
    );
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn model_not_running_is_an_outage_too(pool: PgPool) {
    // Nothing listens on port 9: connection refused.
    let t = test_app(
        pool,
        |c| {
            c.job_max_attempts = 1;
            c.llm_base_url = "http://127.0.0.1:9/v1".into();
        },
        None,
    )
    .await;
    t.connect_owner().await;
    t.mock_message(gmail_message(
        "m9",
        "Dan <dan@example.org>",
        "Hi",
        &["INBOX"],
        "Hi",
    ))
    .await;
    let id = t.insert_event("m9").await;
    t.drain().await;
    assert_eq!(t.event_status(id).await, "triaged");
    let status: String =
        sqlx::query_scalar("SELECT status FROM jobs WHERE subscriber = 'summarize'")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(status, "pending");
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn real_llm_errors_end_dead_without_a_ping(pool: PgPool) {
    let t = test_app(pool, |c| c.job_max_attempts = 1, None).await;
    t.connect_owner().await;
    t.mock_message(gmail_message(
        "m10",
        "Eve <eve@example.org>",
        "Hi",
        &["INBOX"],
        "Hi",
    ))
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&t.server)
        .await;
    let id = t.insert_event("m10").await;
    t.drain().await;
    assert_eq!(t.event_status(id).await, "triaged");
    assert!(t.sent().is_empty());
    let (status, err): (String, Option<String>) =
        sqlx::query_as("SELECT status, last_error FROM jobs WHERE subscriber = 'summarize'")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(status, "dead");
    assert!(err.unwrap().contains("400"));
}
