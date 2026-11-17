//! Shared test helpers. Tests never call real Google, Telegram or LLM endpoints: everything HTTP
//! goes to a local `wiremock` server.
//!
//! Keep each test file below 10 `#[sqlx::test]`s: the sqlx 0.8 harness leaks master-pool permits
//! and stalls for 30 s at cleanup beyond that.
#![allow(dead_code)]

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Local, NaiveDate, TimeZone};
use serde_json::{Value, json};
use sqlx::PgPool;
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use gm::app::{self, App, Overrides};
use gm::clock::FixedClock;
use gm::config::Config;
use gm::notify::RecordingNotifier;

pub const OWNER: &str = "owner@example.com";

pub fn b64(s: &str) -> String {
    URL_SAFE_NO_PAD.encode(s.as_bytes())
}

pub fn config(server: &MockServer) -> Config {
    let mut cfg = Config::for_tests();
    let uri = server.uri();
    cfg.google_auth_url = format!("{uri}/auth");
    cfg.google_token_url = format!("{uri}/token");
    cfg.gmail_api_base = format!("{uri}/gmail/v1");
    cfg.llm_base_url = format!("{uri}/v1");
    cfg.telegram_api_base = uri;
    cfg.rules_path = format!("{}/../../rules.yaml", env!("CARGO_MANIFEST_DIR")).into();
    cfg
}

/// A local time on a fixed day one week ahead of the real date, so that a job deferred from the
/// fake clock ("tomorrow 09:00") is still in the future for Postgres' real `now()`.
pub fn local(h: u32, m: u32) -> chrono::DateTime<Local> {
    let d = test_day().and_hms_opt(h, m, 0).unwrap();
    Local.from_local_datetime(&d).earliest().unwrap()
}

pub fn test_day() -> NaiveDate {
    Local::now().date_naive() + chrono::Duration::days(7)
}

pub struct TestApp {
    pub app: App,
    pub server: MockServer,
    pub notifier: Arc<RecordingNotifier>,
}

/// Build the app against wiremock. `day_window` = Some((now, on, off)) enables model hours.
pub async fn test_app(
    pool: PgPool,
    configure: impl FnOnce(&mut Config),
    now: Option<chrono::DateTime<Local>>,
) -> TestApp {
    let server = MockServer::start().await;
    let mut cfg = config(&server);
    configure(&mut cfg);
    let notifier = Arc::new(RecordingNotifier::default());
    let app = app::build(
        cfg,
        pool,
        Overrides {
            notifier: Some(notifier.clone()),
            clock: now.map(|t| Arc::new(FixedClock(t)) as Arc<dyn gm::clock::Clock>),
        },
    )
    .unwrap();
    TestApp {
        app,
        server,
        notifier,
    }
}

impl TestApp {
    pub async fn connect_owner(&self) {
        self.app
            .state
            .accounts
            .save(
                OWNER,
                OWNER,
                "refresh-token",
                &["https://www.googleapis.com/auth/gmail.readonly".to_string()],
            )
            .await
            .unwrap();
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": "at", "expires_in": 3600})),
            )
            .mount(&self.server)
            .await;
    }

    pub async fn mock_message(&self, msg: Value) {
        let id = msg["id"].as_str().unwrap().to_string();
        Mock::given(method("GET"))
            .and(path(format!("/gmail/v1/users/me/messages/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(msg))
            .mount(&self.server)
            .await;
    }

    pub async fn mock_llm(&self, content: &str) {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"role": "assistant", "content": content}}],
                "usage": {"prompt_tokens": 100, "completion_tokens": 20}
            })))
            .mount(&self.server)
            .await;
    }

    /// Answer only requests whose prompt contains `marker` (e.g. [`TRIAGE`]). Mount these before a
    /// catch-all [`TestApp::mock_llm`]: wiremock picks the first matching mock.
    pub async fn mock_llm_for(&self, marker: &str, content: &str) {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_string_contains(marker))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"role": "assistant", "content": content}}],
                "usage": {"prompt_tokens": 100, "completion_tokens": 20}
            })))
            .mount(&self.server)
            .await;
    }

    /// Run bus jobs until the queue is idle (bounded).
    pub async fn drain(&self) -> usize {
        let mut n = 0;
        while self.app.bus.run_once().await.unwrap() {
            n += 1;
            assert!(n < 100, "bus did not drain");
        }
        n
    }

    pub async fn insert_event(&self, message_id: &str) -> i64 {
        let mut tx = self.app.pool.begin().await.unwrap();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO events (account, source, kind, external_id) VALUES ($1, 'gmail', 'message.received', $2) RETURNING id",
        )
        .bind(OWNER)
        .bind(message_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let payload = json!({"event_id": id, "account": OWNER, "message_id": message_id, "thread_id": format!("t-{message_id}")});
        self.app
            .bus
            .publish(&mut tx, gm::topics::GMAIL_MESSAGE_RECEIVED, &payload)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        id
    }

    pub async fn event_status(&self, id: i64) -> String {
        sqlx::query_scalar("SELECT status FROM events WHERE id = $1")
            .bind(id)
            .fetch_one(&self.app.pool)
            .await
            .unwrap()
    }

    pub fn sent(&self) -> Vec<gm::notify::OutgoingMessage> {
        self.notifier.sent.lock().unwrap().clone()
    }
}

pub fn gmail_message(id: &str, from: &str, subject: &str, labels: &[&str], body: &str) -> Value {
    json!({
        "id": id,
        "threadId": format!("t-{id}"),
        "labelIds": labels,
        "snippet": body.chars().take(80).collect::<String>(),
        "payload": {
            "mimeType": "text/plain",
            "headers": [
                {"name": "From", "value": from},
                {"name": "To", "value": OWNER},
                {"name": "Subject", "value": subject},
                {"name": "Date", "value": "Mon, 16 Nov 2026 10:00:00 +0900"},
                {"name": "Content-Type", "value": "text/plain; charset=UTF-8"}
            ],
            "body": {"data": b64(body)}
        }
    })
}

pub async fn mock_404(server: &MockServer, path_re: &str) {
    Mock::given(method("GET"))
        .and(path_regex(path_re))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

pub const SUMMARY_JSON: &str = r#"{"summary": "Tóm tắt.", "importance": "high", "category": "work", "suggested_actions": ["reply"], "language": "English"}"#;

/// Prompt markers for [`TestApp::mock_llm_for`].
pub const TRIAGE: &str = "Task: decide how the owner should receive";
pub const SUMMARY: &str = "Task: summarize one email for a phone notification";

pub fn triage_json(action: &str, reason: &str) -> String {
    format!(r#"{{"action": "{action}", "reason": "{reason}"}}"#)
}
