//! Task runtime (workstream 3B): routing, the task lifecycle, approvals (hash, expiry, double tap)
//! and the model-unavailable path.
mod common;

use common::*;
use sqlx::PgPool;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const MAIL: &str = "Dear customer,\n\n会議は火曜日の10時からです。\n\nRegards";

async fn with_mail(pool: PgPool) -> (TestApp, i64) {
    let t = test_app(pool, telegram, None).await;
    t.connect_owner().await;
    t.mock_message(gmail_message_with(
        "m1",
        "Alice <alice@example.com>",
        "Meeting",
        MAIL,
        &[("Message-ID", "<abc@mail.example.com>")],
    ))
    .await;
    let id = t.notified_event("m1", "Meeting").await;
    (t, id)
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn translate_button_replies_under_the_notification(pool: PgPool) {
    let (t, id) = with_mail(pool).await;
    t.mock_llm_for(
        TRANSLATE,
        r#"{"translation": "Subject: Cuộc họp\n\nCuộc họp lúc 10 giờ thứ Ba."}"#,
    )
    .await;
    t.update(tap_update(
        1,
        42,
        NOTIFICATION_MSG,
        &format!("i:{id}:translate"),
    ))
    .await;
    // A second tap while the first is queued does not start a second translation.
    t.update(tap_update(
        2,
        42,
        NOTIFICATION_MSG,
        &format!("i:{id}:translate"),
    ))
    .await;
    t.drain().await;
    let sent = t.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].text.starts_with("🌐 Translation (Vietnamese)"));
    assert!(sent[0].text.contains("10 giờ thứ Ba"));
    assert_eq!(sent[0].reply_to, Some(NOTIFICATION_MSG));
    let (status, result, _) = t.task_row(t.last_task().await).await;
    assert_eq!(status, "done");
    assert!(result["text"].as_str().unwrap().contains("thứ Ba"));
    // Answers are mapped too, so replying to the translation still means this mail.
    let refs = t.app.tasks.chat.lookup(1).await.unwrap().unwrap();
    assert_eq!(refs.event_id, Some(id));
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn words_go_through_the_router_when_no_keyword_matches(pool: PgPool) {
    let (t, _) = with_mail(pool).await;
    t.mock_llm_for(ROUTER, r#"{"intent": "translate", "language": "Japanese"}"#)
        .await;
    t.mock_llm_for(TRANSLATE, r#"{"translation": "件名: 会議"}"#)
        .await;
    t.update(text_update(
        1,
        42,
        "could you put this into Japanese?",
        Some(NOTIFICATION_MSG),
    ))
    .await;
    t.drain().await;
    assert!(t.last_sent().text.starts_with("🌐 Translation (Japanese)"));
    assert_eq!(t.last_sent().reply_to, Some(5001));
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn unclear_words_get_the_usage_hint(pool: PgPool) {
    let (t, _) = with_mail(pool).await;
    t.mock_llm_for(ROUTER, r#"{"intent": "unknown", "language": ""}"#)
        .await;
    t.update(text_update(1, 42, "hmm", Some(NOTIFICATION_MSG)))
        .await;
    t.drain().await;
    assert!(
        t.last_sent()
            .text
            .starts_with("I can translate, analyze or reply")
    );
    assert_eq!(t.task_row(t.last_task().await).await.0, "done");
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn while_the_model_is_down_the_owner_is_told_once_and_the_task_waits(pool: PgPool) {
    let (t, id) = with_mail(pool).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&t.server)
        .await;
    t.update(tap_update(
        1,
        42,
        NOTIFICATION_MSG,
        &format!("i:{id}:analyze"),
    ))
    .await;
    t.drain().await;
    assert_eq!(t.sent().len(), 1);
    assert!(t.sent()[0].text.contains("isn't answering"));
    let (status, run_after): (String, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT status, run_after FROM jobs WHERE subscriber = 'task_run'")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(status, "pending");
    assert!(run_after > chrono::Utc::now() + chrono::Duration::seconds(200));
    assert_eq!(t.task_row(t.last_task().await).await.0, "running");
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn a_payload_that_no_longer_matches_its_hash_is_refused(pool: PgPool) {
    let (t, id) = with_mail(pool).await;
    t.mock_draft_api(draft_json(
        "d1",
        "t-m1",
        "alice@example.com",
        "Re: Meeting",
        "Tuesday works.",
    ))
    .await;
    let a = t.proposed_send(id).await;
    sqlx::query(
        "UPDATE approvals SET payload = jsonb_set(payload, '{to}', '[\"x@evil.example\"]') WHERE id = $1",
    )
    .bind(a)
    .execute(&t.app.pool)
    .await
    .unwrap();
    t.update(tap_update(1, 42, 300, &format!("a:{a}:y"))).await;
    t.drain().await;
    assert_eq!(t.approval_status(a).await, "failed");
    assert_eq!(requests_to(&t.server, "POST", "/drafts/send").await, 0);
    assert!(t.last_sent().text.contains("changed after it was proposed"));
    assert_eq!(t.audit_count().await, 0);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn approvals_expire(pool: PgPool) {
    let (t, id) = with_mail(pool).await;
    let a = t.proposed_send(id).await;
    sqlx::query("UPDATE approvals SET expires_at = now() - interval '1 minute' WHERE id = $1")
        .bind(a)
        .execute(&t.app.pool)
        .await
        .unwrap();
    // Approving after expiry, before the sweep ran: nothing is sent.
    t.update(tap_update(1, 42, 300, &format!("a:{a}:y"))).await;
    t.drain().await;
    assert_eq!(t.approval_status(a).await, "expired");
    assert_eq!(requests_to(&t.server, "POST", "/drafts/send").await, 0);

    let b = proposed_again(&t, id).await;
    assert_eq!(
        gm::tasks::approvals::expire_due(&t.app.pool, &t.app.bus)
            .await
            .unwrap(),
        1
    );
    t.drain().await;
    assert_eq!(t.approval_status(b).await, "expired");
    let edits = t.notifier.button_edits.lock().unwrap().clone();
    assert!(edits.contains(&(300, vec![])), "buttons removed: {edits:?}");
}

async fn proposed_again(t: &TestApp, event_id: i64) -> i64 {
    sqlx::query("UPDATE tasks SET origin_key = 'cb:old'")
        .execute(&t.app.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM chat_messages WHERE message_id = 300")
        .execute(&t.app.pool)
        .await
        .unwrap();
    let b = t.proposed_send(event_id).await;
    sqlx::query("UPDATE approvals SET expires_at = now() - interval '1 minute' WHERE id = $1")
        .bind(b)
        .execute(&t.app.pool)
        .await
        .unwrap();
    b
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn a_double_tap_sends_once(pool: PgPool) {
    let (t, id) = with_mail(pool).await;
    t.mock_draft_api(draft_json(
        "d1",
        "t-m1",
        "alice@example.com",
        "Re: Meeting",
        "Tuesday works.",
    ))
    .await;
    let a = t.proposed_send(id).await;
    t.update(tap_update(1, 42, 300, &format!("a:{a}:y"))).await;
    t.update(tap_update(2, 42, 300, &format!("a:{a}:y"))).await;
    let r = gm::web::router(t.app.state.clone());
    let cookie = session_cookie(&t, OWNER);
    send(
        &r,
        post_form(&format!("/approvals/{a}/approve"), &cookie, ""),
    )
    .await;
    t.drain().await;
    assert_eq!(requests_to(&t.server, "POST", "/drafts/send").await, 1);
    assert_eq!(t.approval_status(a).await, "executed");
    assert_eq!(t.audit_count().await, 1);
    assert!(
        t.last_sent()
            .text
            .starts_with("✅ Sent to alice@example.com")
    );
}
