mod common;

use chrono::Timelike;
use common::*;
use serde_json::json;
use sqlx::PgPool;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

fn day_hours(cfg: &mut gm::config::Config) {
    cfg.model_on = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    cfg.model_off = chrono::NaiveTime::from_hms_opt(19, 0, 0).unwrap();
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn unmatched_mail_triaged_notify_is_summarized_and_notified(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    t.connect_owner().await;
    t.mock_message(gmail_message(
        "m1",
        "Alice <alice@example.org>",
        "Lunch?",
        &["INBOX"],
        "Shall we have lunch tomorrow?",
    ))
    .await;
    t.mock_llm_for(
        TRIAGE,
        &triage_json("notify", "a person asks you something"),
    )
    .await;
    t.mock_llm(SUMMARY_JSON).await;

    let id = t.insert_event("m1").await;
    assert_eq!(t.drain().await, 4, "triage, classify, summarize, notify");
    assert_eq!(t.event_status(id).await, "notified");
    let sent = t.sent();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].text.contains("🔴 📧 Lunch?"));
    assert!(sent[0].text.contains("From: Alice <alice@example.org>"));
    assert!(sent[0].text.contains("Tóm tắt."));
    assert!(sent[0].text.contains("Suggested: reply"));
    assert!(!sent[0].silent);

    let ev = gm::db::event(&t.app.pool, id).await.unwrap().unwrap();
    assert_eq!(ev.rule.as_deref(), Some("model"));
    assert_eq!(ev.data["verdict"]["reason"], "a person asks you something");
    assert_eq!(ev.summary.unwrap()["importance"], "high");
    let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_calls WHERE ok")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(calls, 2, "triage + summary");

    // At-least-once: redelivering the same jobs changes nothing.
    sqlx::query("UPDATE jobs SET status = 'pending'")
        .execute(&t.app.pool)
        .await
        .unwrap();
    t.drain().await;
    assert_eq!(t.sent().len(), 1);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn card_flash_is_a_silent_rule_only_ping_even_at_night(pool: PgPool) {
    let t = test_app(pool, day_hours, Some(local(2, 0))).await;
    t.connect_owner().await;
    t.mock_message(gmail_message(
        "m2",
        "楽天カード株式会社 <info@mail.rakuten-card.co.jp>",
        "【速報版】カード利用のお知らせ(本人ご利用分)",
        &["INBOX", "CATEGORY_UPDATES"],
        "ご利用日 利用者 ご利用金額 2026/11/15 本人 980 円",
    ))
    .await;
    let id = t.insert_event("m2").await;
    assert_eq!(t.drain().await, 2, "triage + notify, no summarize");
    assert_eq!(t.event_status(id).await, "notified");
    let sent = t.sent();
    assert!(sent[0].silent);
    assert!(
        sent[0]
            .text
            .starts_with("🔕 💳 Rakuten Card ¥980 (2026/11/15)"),
        "{}",
        sent[0].text
    );
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn night_mail_needing_the_model_is_deferred_to_morning(pool: PgPool) {
    let t = test_app(pool, day_hours, Some(local(22, 0))).await;
    t.connect_owner().await;
    t.mock_message(gmail_message(
        "m3",
        "Bob <bob@example.org>",
        "Contract",
        &["INBOX"],
        "Please sign.",
    ))
    .await;
    let id = t.insert_event("m3").await;
    t.drain().await;
    assert_eq!(t.event_status(id).await, "classify");
    assert!(t.sent().is_empty());
    let (status, attempts, run_after): (String, i32, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as(
            "SELECT status, attempts, run_after FROM jobs WHERE subscriber = 'classify'",
        )
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!((status.as_str(), attempts), ("pending", 0));
    let local_run = run_after.with_timezone(&chrono::Local);
    assert_eq!((local_run.hour(), local_run.minute()), (9, 0));
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn at_night_only_rule_only_pings_go_out(pool: PgPool) {
    let t = test_app(pool, day_hours, Some(local(23, 30))).await;
    t.connect_owner().await;
    // Urgent but needs a summary: waits for 09:00 (no summary-less duplicate of the Gmail ping).
    t.mock_message(gmail_message(
        "m4",
        "Google <no-reply@accounts.google.com>",
        "Security alert",
        &["INBOX"],
        "New sign-in",
    ))
    .await;
    // Urgent rule-only (summarize: false): needs no model, goes out now.
    t.mock_message(gmail_message(
        "m4b",
        "Rakuten <no-reply@id.rakuten.co.jp>",
        "[楽天] ログイン通知",
        &["INBOX"],
        "login",
    ))
    .await;
    let (security, login) = (t.insert_event("m4").await, t.insert_event("m4b").await);
    t.drain().await;
    assert_eq!(t.event_status(security).await, "triaged");
    assert_eq!(t.event_status(login).await, "notified");
    let sent = t.sent();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].text.starts_with("🚨 📧 [楽天] ログイン通知"),
        "{}",
        sent[0].text
    );
    assert!(!sent[0].silent);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn digest_ignore_and_deleted_mail(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    t.connect_owner().await;
    t.mock_message(gmail_message(
        "m5",
        "LinkedIn Job Alerts <jobalerts-noreply@linkedin.com>",
        "new jobs",
        &["INBOX"],
        "x",
    ))
    .await;
    t.mock_message(gmail_message(
        "m6",
        "楽天みんなのレビュー <hyouka@rakuten.co.jp>",
        "レビュー",
        &["INBOX"],
        "x",
    ))
    .await;
    mock_404(&t.server, r"^/gmail/v1/users/me/messages/m7$").await;
    let (a, b, c) = (
        t.insert_event("m5").await,
        t.insert_event("m6").await,
        t.insert_event("m7").await,
    );
    t.drain().await;
    assert_eq!(t.event_status(a).await, "digest");
    assert_eq!(t.event_status(b).await, "ignored");
    assert_eq!(t.event_status(c).await, "gone");
    assert!(t.sent().is_empty());
    let ev = gm::db::event(&t.app.pool, a).await.unwrap().unwrap();
    assert_eq!(ev.data["verdict"]["section"], "jobs");
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn ingest_creates_events_once_and_recovers_expired_history(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    t.connect_owner().await;
    sqlx::query("INSERT INTO gmail_state (email, history_id) VALUES ($1, 100)")
        .bind(OWNER)
        .execute(&t.app.pool)
        .await
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "historyId": "120",
            "history": [
                {"messagesAdded": [{"message": {"id": "a", "threadId": "ta", "labelIds": ["INBOX"]}}]},
                {"messagesAdded": [{"message": {"id": "a", "threadId": "ta", "labelIds": ["INBOX"]}},
                                    {"message": {"id": "s", "threadId": "ts", "labelIds": ["SENT"]}}]}
            ]
        })))
        .up_to_n_times(2)
        .mount(&t.server)
        .await;
    assert_eq!(t.app.ingestor.sync(OWNER).await.unwrap(), 1);
    assert_eq!(
        t.app.ingestor.sync(OWNER).await.unwrap(),
        0,
        "dedupe by (source, external_id)"
    );
    let hid: i64 = sqlx::query_scalar("SELECT history_id FROM gmail_state")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(hid, 120);

    // Cursor too old → 404 → recover recent INBOX mail and re-anchor.
    t.server.reset().await;
    t.connect_owner().await;
    mock_404(&t.server, r"^/gmail/v1/users/me/history$").await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"historyId": "500"})))
        .mount(&t.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"messages": [{"id": "b", "threadId": "tb"}, {"id": "a", "threadId": "ta"}]}),
        ))
        .mount(&t.server)
        .await;
    assert_eq!(
        t.app.ingestor.sync(OWNER).await.unwrap(),
        1,
        "only b is new"
    );
    let hid: i64 = sqlx::query_scalar("SELECT history_id FROM gmail_state")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(hid, 500);
    let jobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE topic = 'gmail.message.received'")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(jobs, 2);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn rejected_refresh_token_marks_account_for_reconnect(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    t.app
        .state
        .accounts
        .save(OWNER, OWNER, "old", &[])
        .await
        .unwrap();
    sqlx::query("INSERT INTO gmail_state (email, history_id) VALUES ($1, 1)")
        .bind(OWNER)
        .execute(&t.app.pool)
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})))
        .mount(&t.server)
        .await;
    assert!(t.app.ingestor.sync(OWNER).await.is_err());
    let acc = t.app.state.accounts.get(OWNER).await.unwrap().unwrap();
    assert_eq!(acc.status, "needs_reconnect");
    let err: Option<String> = sqlx::query_scalar("SELECT last_error FROM gmail_state")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert!(err.unwrap().contains("reconnected"));
}
