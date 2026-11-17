//! Pub/Sub ingest mode (ADR-0002) against a wiremock Google.
mod common;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::*;
use gm::config::PubSubConfig;
use serde_json::json;
use sqlx::PgPool;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

const TOPIC: &str = "/pubsub/v1/projects/proj/topics/gm-gmail";
const SUB: &str = "/pubsub/v1/projects/proj/subscriptions/gm-gmail-pull";

async fn pubsub_app(pool: PgPool, scopes: &[&str]) -> TestApp {
    let t = test_app(
        pool,
        |c| {
            c.pubsub = Some(PubSubConfig {
                project_id: "proj".into(),
                topic: "gm-gmail".into(),
                subscription: "gm-gmail-pull".into(),
                api_base: format!(
                    "{}/pubsub/v1",
                    c.gmail_api_base.trim_end_matches("/gmail/v1")
                ),
                safety_poll_interval_s: 900,
            })
        },
        None,
    )
    .await;
    t.connect_owner().await; // mocks /token
    let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
    t.app
        .state
        .accounts
        .save(OWNER, OWNER, "refresh-token", &scopes)
        .await
        .unwrap();
    sqlx::query("INSERT INTO gmail_state (email, history_id) VALUES ($1, 100)")
        .bind(OWNER)
        .execute(&t.app.pool)
        .await
        .unwrap();
    t
}

fn notification(email: &str) -> String {
    STANDARD.encode(json!({"emailAddress": email, "historyId": 101}).to_string())
}

async fn mock_pull(t: &TestApp) {
    Mock::given(method("POST"))
        .and(path(format!("{SUB}:pull")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"receivedMessages": [
                {"ackId": "ack-1", "message": {"data": notification(OWNER)}},
                {"ackId": "ack-2", "message": {"data": notification("stranger@example.com")}},
                {"ackId": "ack-3", "message": {"data": "not json"}},
            ]})),
        )
        .expect(1)
        .mount(&t.server)
        .await;
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn provisions_watches_pulls_syncs_and_acks(pool: PgPool) {
    let t = pubsub_app(pool, &[gm::google::GMAIL_READONLY, gm::google::PUBSUB]).await;
    let s = &t.server;
    // Topic already exists (409), the publisher binding is missing, the subscription is new.
    Mock::given(method("PUT"))
        .and(path(TOPIC))
        .respond_with(ResponseTemplate::new(409))
        .expect(1)
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{TOPIC}:getIamPolicy")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"etag": "e1"})))
        .mount(s)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{TOPIC}:setIamPolicy")))
        .and(body_string_contains(
            "gmail-api-push@system.gserviceaccount.com",
        ))
        .and(body_string_contains("roles/pubsub.publisher"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(s)
        .await;
    Mock::given(method("PUT"))
        .and(path(SUB))
        .and(body_string_contains("projects/proj/topics/gm-gmail"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(s)
        .await;
    let expiration = (chrono::Utc::now() + chrono::Duration::days(7)).timestamp_millis();
    Mock::given(method("POST"))
        .and(path("/gmail/v1/users/me/watch"))
        .and(body_string_contains("\"labelIds\":[\"INBOX\"]"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"historyId": "100", "expiration": expiration.to_string()})),
        )
        .expect(2) // first watch + one renewal below
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "history": [{"messagesAdded": [{"message": {"id": "m1", "threadId": "t1", "labelIds": ["INBOX"]}}]}],
            "historyId": "101"
        })))
        .expect(1)
        .mount(s)
        .await;
    mock_pull(&t).await;
    Mock::given(method("POST"))
        .and(path(format!("{SUB}:acknowledge")))
        .and(body_string_contains("ack-1"))
        .and(body_string_contains("ack-2"))
        .and(body_string_contains("ack-3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(s)
        .await;

    let ps = t.app.pubsub.clone().expect("pubsub mode");
    let email = ps.prepare().await.unwrap();
    assert_eq!(email, OWNER);
    let stored: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT watch_expiration FROM gmail_state WHERE email = $1")
            .bind(OWNER)
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(stored.unwrap().timestamp_millis(), expiration);

    // Not due: no call. Due within six days: renewed.
    ps.ensure_watches().await.unwrap();
    sqlx::query("UPDATE gmail_state SET watch_expiration = now() + interval '1 day'")
        .execute(&t.app.pool)
        .await
        .unwrap();
    ps.ensure_watches().await.unwrap();

    // Only the owner's mailbox is synced; unknown and malformed notifications are acked too.
    assert_eq!(ps.step(&email).await.unwrap(), 3);
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(events, 1);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn failed_sync_leaves_the_batch_for_redelivery(pool: PgPool) {
    let t = pubsub_app(pool, &[gm::google::GMAIL_READONLY, gm::google::PUBSUB]).await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/history"))
        .respond_with(ResponseTemplate::new(500).set_body_string("backend error"))
        .mount(&t.server)
        .await;
    mock_pull(&t).await;
    Mock::given(method("POST"))
        .and(path(format!("{SUB}:acknowledge")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(0)
        .mount(&t.server)
        .await;
    let ps = t.app.pubsub.clone().unwrap();
    assert_eq!(ps.step(OWNER).await.unwrap(), 0);
    let err: Option<String> = sqlx::query_scalar("SELECT last_error FROM gmail_state")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert!(err.unwrap().contains("500"));
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn without_the_pubsub_scope_it_asks_for_a_reconnect(pool: PgPool) {
    let t = pubsub_app(pool, &[gm::google::GMAIL_READONLY]).await;
    let err = t.app.pubsub.clone().unwrap().prepare().await.unwrap_err();
    assert!(err.to_string().contains("reconnect"), "{err}");

    // The consent screen asks for Pub/Sub in this mode, and the dashboard shows the fallback.
    let scopes = t.app.cfg.required_scopes();
    assert!(scopes.contains(&gm::google::PUBSUB));
    let url =
        gm::google::oauth::authorize_url(&t.app.cfg, gm::google::oauth::Flow::Connect, "s", "v");
    assert!(url.contains("auth%2Fpubsub"), "{url}");
    t.app.ingestor.set_pubsub_error(&err.to_string());
    assert!(!t.app.ingestor.pubsub_ok());
    assert!(
        !gm::config::Config::for_tests()
            .required_scopes()
            .contains(&gm::google::PUBSUB)
    );
}
