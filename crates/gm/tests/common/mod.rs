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
    cfg.llm_ready_url = format!("{uri}/ready");
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
    test_app_with(pool, configure, now, None).await
}

/// [`test_app`] with a fake model process (instead of the `mlx-air` CLI).
pub async fn test_app_with(
    pool: PgPool,
    configure: impl FnOnce(&mut Config),
    now: Option<chrono::DateTime<Local>>,
    model_process: Option<Arc<dyn gm::model::ModelProcess>>,
) -> TestApp {
    let server = MockServer::start().await;
    // The model is up unless a test says otherwise (low priority: `mock_ready` overrides it).
    Mock::given(method("GET"))
        .and(path("/ready"))
        .respond_with(ResponseTemplate::new(200))
        .with_priority(10)
        .mount(&server)
        .await;
    let mut cfg = config(&server);
    configure(&mut cfg);
    let notifier = Arc::new(RecordingNotifier::default());
    let app = app::build(
        cfg,
        pool,
        Overrides {
            notifier: Some(notifier.clone()),
            clock: now.map(|t| Arc::new(FixedClock(t)) as Arc<dyn gm::clock::Clock>),
            model_process,
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

// ------------------------------------------------------------------------------ web helpers

pub async fn send(
    router: &axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (axum::http::StatusCode, axum::http::HeaderMap, String) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let resp = router.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        parts.headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

pub fn get(uri: &str, cookie: Option<&str>) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::get(uri).header(axum::http::header::HOST, "localhost:8080");
    if let Some(c) = cookie {
        b = b.header(axum::http::header::COOKIE, c);
    }
    b.body(axum::body::Body::empty()).unwrap()
}

pub fn post_form(uri: &str, cookie: &str, form: &str) -> axum::http::Request<axum::body::Body> {
    use axum::http::header;
    axum::http::Request::post(uri)
        .header(header::HOST, "localhost:8080")
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(form.to_string()))
        .unwrap()
}

pub fn session_cookie(t: &TestApp, email: &str) -> String {
    use gm::web::session::{Session, Signer};
    let now = chrono::Utc::now().timestamp();
    let tok = Signer::new(&t.app.cfg.session_secret).sign(
        "session",
        &Session {
            email: email.into(),
            iat: now,
            exp: now + 600,
        },
    );
    format!("gm_session={tok}")
}

pub async fn mock_ready(server: &MockServer, ok: bool) {
    Mock::given(method("GET"))
        .and(path("/ready"))
        .respond_with(ResponseTemplate::new(if ok { 200 } else { 503 }))
        .mount(server)
        .await;
}

// ------------------------------------------------------------------------- Phase 3 helpers

/// Telegram configured with token `T` and chat `42` (the poller exists).
pub fn telegram(cfg: &mut Config) {
    cfg.telegram_bot_token = Some("T".into());
    cfg.telegram_chat_id = Some("42".into());
}

/// Telegram message id of the notification made by [`TestApp::notified_event`].
pub const NOTIFICATION_MSG: i64 = 900;

/// Prompt markers of the Phase 3 tasks.
pub const ROUTER: &str = "Task: tell what the owner wants";
pub const TRANSLATE: &str = "Task: translate one part of an email";
pub const ANALYZE: &str = "Task: analyze one email";
pub const REPLY: &str = "Task: write the body of the owner's reply";

/// A button press from chat `chat` on message `message_id`.
pub fn tap_update(update_id: i64, chat: i64, message_id: i64, data: &str) -> Value {
    json!({"update_id": update_id, "callback_query": {
        "id": format!("cq{update_id}"), "from": {"id": chat}, "data": data,
        "message": {"message_id": message_id, "chat": {"id": chat}}}})
}

/// A text message from chat `chat`, optionally replying to `reply_to`.
pub fn text_update(update_id: i64, chat: i64, text: &str, reply_to: Option<i64>) -> Value {
    let mut m = json!({"message_id": 5000 + update_id, "chat": {"id": chat}, "from": {"id": chat}, "text": text});
    if let Some(r) = reply_to {
        m["reply_to_message"] = json!({"message_id": r, "chat": {"id": chat}});
    }
    json!({"update_id": update_id, "message": m})
}

/// A Gmail draft as `drafts.get` returns it.
pub fn draft_json(id: &str, thread: &str, to: &str, subject: &str, body: &str) -> Value {
    json!({"id": id, "message": {
        "id": format!("m-{id}"), "threadId": thread, "labelIds": ["DRAFT"],
        "payload": {"mimeType": "text/plain", "headers": [
            {"name": "To", "value": to}, {"name": "Subject", "value": subject},
            {"name": "Content-Type", "value": "text/plain; charset=UTF-8"}],
            "body": {"data": b64(body)}}}})
}

/// The decoded RFC 5322 text of every `drafts.create` / `drafts.update` request.
pub async fn draft_requests(server: &MockServer) -> Vec<(String, Value)> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.url.path().contains("/drafts") && !r.url.path().ends_with("/send"))
        .filter(|r| r.method.as_str() != "GET")
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            let raw = body["message"]["raw"].as_str().unwrap_or_default();
            let text = String::from_utf8(URL_SAFE_NO_PAD.decode(raw).unwrap()).unwrap();
            (text, body)
        })
        .collect()
}

pub async fn requests_to(server: &MockServer, method_name: &str, suffix: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == method_name && r.url.path().ends_with(suffix))
        .count()
}

impl TestApp {
    pub fn poller(&self) -> Arc<gm::telegram::TelegramPoller> {
        self.app.telegram.clone().expect("Telegram configured")
    }

    /// An event for Gmail message `message_id` that was notified as Telegram message
    /// [`NOTIFICATION_MSG`].
    pub async fn notified_event(&self, message_id: &str, subject: &str) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO events (account, source, kind, external_id, status, rule, data)
             VALUES ($1, 'gmail', 'message.received', $2, 'notified', 'model', $3) RETURNING id",
        )
        .bind(OWNER)
        .bind(message_id)
        .bind(json!({"subject": subject, "from": "Alice <alice@example.com>"}))
        .fetch_one(&self.app.pool)
        .await
        .unwrap();
        self.app
            .tasks
            .chat
            .record_out(
                NOTIFICATION_MSG,
                gm::telegram::MessageRefs {
                    event_id: Some(id),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        id
    }

    pub async fn update(&self, u: Value) {
        self.poller().handle_update(&u).await.unwrap();
    }

    pub async fn mock_draft_api(&self, get: Value) {
        Mock::given(method("POST"))
            .and(path("/gmail/v1/users/me/drafts"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id": "d1", "message": {"id": "dm1"}})),
            )
            .mount(&self.server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/gmail/v1/users/me/drafts/d1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id": "d1", "message": {"id": "dm2"}})),
            )
            .mount(&self.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/gmail/v1/users/me/drafts/d1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(get))
            .mount(&self.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/gmail/v1/users/me/drafts/send"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"id": "sent1", "threadId": "t-m1", "labelIds": ["SENT"]}),
                ),
            )
            .mount(&self.server)
            .await;
    }

    pub async fn task_row(&self, id: i64) -> (String, Value, Option<String>) {
        sqlx::query_as("SELECT status, result, error FROM tasks WHERE id = $1")
            .bind(id)
            .fetch_one(&self.app.pool)
            .await
            .unwrap()
    }

    pub async fn last_task(&self) -> i64 {
        sqlx::query_scalar("SELECT max(id) FROM tasks")
            .fetch_one(&self.app.pool)
            .await
            .unwrap()
    }

    pub async fn approval_status(&self, id: i64) -> String {
        sqlx::query_scalar("SELECT status FROM approvals WHERE id = $1")
            .bind(id)
            .fetch_one(&self.app.pool)
            .await
            .unwrap()
    }

    pub async fn last_approval(&self) -> i64 {
        sqlx::query_scalar("SELECT max(id) FROM approvals")
            .fetch_one(&self.app.pool)
            .await
            .unwrap()
    }

    pub async fn audit_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM audit_log")
            .fetch_one(&self.app.pool)
            .await
            .unwrap()
    }

    /// A task awaiting approval of a `gmail.send_draft`, as the reply recipe leaves it.
    pub async fn proposed_send(&self, event_id: i64) -> i64 {
        let task_id: i64 = sqlx::query_scalar(
            "INSERT INTO tasks (origin_key, event_id, origin, instruction, intent, status, result)
             VALUES ('cb:x', $1, 'telegram', 'ok', 'reply', 'awaiting_approval', '{\"draft_id\": \"d1\"}') RETURNING id",
        )
        .bind(event_id)
        .fetch_one(&self.app.pool)
        .await
        .unwrap();
        let draft = gm::google::compose::ReplyDraft {
            to: vec!["alice@example.com".into()],
            subject: "Re: Meeting".into(),
            in_reply_to: String::new(),
            references: String::new(),
            thread_id: "t-m1".into(),
            body: "Tuesday works.".into(),
        };
        let payload = gm::recipes::reply::payload(OWNER, "d1", &draft);
        let mut conn = self.app.pool.acquire().await.unwrap();
        let id = gm::tasks::approvals::create(
            &mut conn,
            task_id,
            gm::recipes::reply::SEND_DRAFT,
            &payload,
            "preview",
            chrono::Utc::now() + chrono::Duration::hours(24),
        )
        .await
        .unwrap();
        self.app
            .tasks
            .chat
            .record_out(
                300,
                gm::telegram::MessageRefs {
                    event_id: Some(event_id),
                    task_id: Some(task_id),
                    approval_id: Some(id),
                },
                None,
            )
            .await
            .unwrap();
        id
    }

    pub fn last_sent(&self) -> gm::notify::OutgoingMessage {
        self.sent().last().cloned().expect("a message was sent")
    }
}

/// A Gmail message with extra headers (e.g. Message-ID, Reply-To).
pub fn gmail_message_with(
    id: &str,
    from: &str,
    subject: &str,
    body: &str,
    extra: &[(&str, &str)],
) -> Value {
    let mut m = gmail_message(id, from, subject, &["INBOX"], body);
    let hs = m["payload"]["headers"].as_array_mut().unwrap();
    for (k, v) in extra {
        hs.push(json!({"name": k, "value": v}));
    }
    m
}

// ------------------------------------------------------------------------- model residency

/// A fake `mlx-air`: `start` and `stop` flip `up`, which the mounted `/ready` mock reports.
#[derive(Default)]
pub struct FakeModel {
    pub up: std::sync::atomic::AtomicBool,
    pub calls: std::sync::Mutex<Vec<String>>,
}

impl FakeModel {
    pub fn new(up: bool) -> Arc<Self> {
        let m = Arc::new(Self::default());
        m.up.store(up, std::sync::atomic::Ordering::SeqCst);
        m
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn is_up(&self) -> bool {
        self.up.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Make `/ready` follow this fake (200 with the model id while up, else 503).
    pub async fn mount_ready(self: &Arc<Self>, server: &MockServer) {
        let me = self.clone();
        Mock::given(method("GET"))
            .and(path("/ready"))
            .respond_with(move |_: &wiremock::Request| {
                if me.is_up() {
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"ready": true, "model": "test-model"}))
                } else {
                    ResponseTemplate::new(503)
                }
            })
            .with_priority(1)
            .mount(server)
            .await;
    }
}

#[async_trait::async_trait]
impl gm::model::ModelProcess for FakeModel {
    async fn start(&self, instance: &str, model: &str, port: u16) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("start {instance} {model} {port}"));
        self.up.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&self, instance: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("stop {instance}"));
        self.up.store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// Model hours 09:00–19:00 with gm managing the model.
pub fn managed_day_hours(cfg: &mut Config) {
    cfg.model_on = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    cfg.model_off = chrono::NaiveTime::from_hms_opt(19, 0, 0).unwrap();
    cfg.model_manage = true;
}
