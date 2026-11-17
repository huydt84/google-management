//! Owner-requested Gmail sync through Telegram, with isolated Google endpoints.
mod common;

use common::*;
use serde_json::json;
use sqlx::PgPool;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn sync_command_ingests_mail_at_night_once_and_keeps_existing_notification_rules(
    pool: PgPool,
) {
    let t = test_app(
        pool,
        |cfg| {
            telegram(cfg);
            cfg.model_on = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
            cfg.model_off = chrono::NaiveTime::from_hms_opt(19, 0, 0).unwrap();
        },
        Some(local(23, 0)),
    )
    .await;
    t.connect_owner().await;
    sqlx::query("INSERT INTO gmail_state (email, history_id) VALUES ($1, 100)")
        .bind(OWNER)
        .execute(&t.app.pool)
        .await
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "historyId": "120", "history": [{"messagesAdded": [
                {"message": {"id": "login", "threadId": "t-login", "labelIds": ["INBOX"]}},
                {"message": {"id": "security", "threadId": "t-security", "labelIds": ["INBOX"]}}
            ]}]
        })))
        .expect(2)
        .mount(&t.server)
        .await;
    t.mock_message(gmail_message(
        "login",
        "Rakuten <no-reply@id.rakuten.co.jp>",
        "Login notice",
        &["INBOX"],
        "login",
    ))
    .await;
    t.mock_message(gmail_message(
        "security",
        "Google <no-reply@accounts.google.com>",
        "Security alert",
        &["INBOX"],
        "security",
    ))
    .await;

    t.update(text_update(1, 99, "/sync", None)).await;
    assert!(t.sent().is_empty(), "foreign chats cannot trigger sync");
    let command = text_update(2, 42, "/sync@MyBot", None);
    t.update(command.clone()).await;
    assert_eq!(t.last_sent().text, "Gmail checked. New emails found: 2.");
    assert_eq!(t.last_sent().reply_to, Some(5002));
    t.update(command).await;
    assert_eq!(t.sent().len(), 1, "a re-polled command is answered once");
    t.drain().await;
    assert_eq!(
        t.sent().len(),
        2,
        "only the rule-only login notice is delivered at night"
    );
    assert!(t.last_sent().text.contains("Login notice"));
    assert!(!t.last_sent().silent, "urgent rule-only mail keeps sound");
    let pending: String =
        sqlx::query_scalar("SELECT status FROM events WHERE external_id = 'security'")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(
        pending, "triaged",
        "summarized urgent mail waits for model hours"
    );

    t.update(text_update(3, 42, "/sync", None)).await;
    assert_eq!(t.last_sent().text, "Gmail checked. New emails found: 0.");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(count, 2, "manual checks never duplicate mail");
    let cursor: i64 = sqlx::query_scalar("SELECT history_id FROM gmail_state")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(cursor, 120);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn sync_command_explains_connection_and_request_failures(pool: PgPool) {
    let t = test_app(pool, telegram, None).await;
    t.update(text_update(1, 42, "/sync", None)).await;
    assert_eq!(
        t.last_sent().text,
        "Connect or reconnect Gmail on the dashboard, then try /sync again."
    );
    t.connect_owner().await;
    sqlx::query("INSERT INTO gmail_state (email, history_id) VALUES ($1, 100)")
        .bind(OWNER)
        .execute(&t.app.pool)
        .await
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/history"))
        .respond_with(ResponseTemplate::new(503).set_body_string("private upstream detail"))
        .mount(&t.server)
        .await;
    t.update(text_update(2, 42, "/sync", None)).await;
    assert_eq!(
        t.last_sent().text,
        "Couldn't check Gmail. Try /sync again later. Details are on the dashboard."
    );
    let error: String = sqlx::query_scalar("SELECT last_error FROM gmail_state")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert!(error.contains("503"));

    t.app
        .state
        .accounts
        .mark_needs_reconnect(OWNER)
        .await
        .unwrap();
    t.update(text_update(3, 42, "/sync", None)).await;
    assert_eq!(
        t.last_sent().text,
        "Connect or reconnect Gmail on the dashboard, then try /sync again."
    );
    t.update(text_update(4, 42, "/help", None)).await;
    assert!(t.last_sent().text.contains("/sync"));
}
