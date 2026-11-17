//! Runs the actual server binary with an isolated database and fake external APIs.
mod common;

use std::process::Stdio;
use std::time::Duration;

use common::*;
use gm::google::CALENDAR_READONLY;
use serde_json::{Value, json};
use sqlx::{ConnectOptions, PgPool};
use tokio::process::Command;
use wiremock::matchers::{body_partial_json, method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn server_delivers_calendar_and_manual_sync_without_automatic_sync_alerts(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    t.connect_owner().await;
    t.app
        .state
        .accounts
        .save(OWNER, OWNER, "refresh-token", &[CALENDAR_READONLY.into()])
        .await
        .unwrap();
    sqlx::query("INSERT INTO calendar_state (email, calendar_id, sync_token) VALUES ($1, 'primary', 'baseline')")
        .bind(OWNER).execute(&t.app.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO gmail_state (email, history_id, last_sync_at) VALUES ($1, 100, now())",
    )
    .bind(OWNER)
    .execute(&t.app.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO health_state (check_name, failing_since, alerted_at, detail) VALUES ('ingest_lag', now() - interval '20 minutes', now() - interval '5 minutes', 'Gmail not synced for 30 min')")
        .execute(&t.app.pool).await.unwrap();
    let event = json!({"id": "live-invitation", "updated": "2026-11-17T01:00:00Z",
        "status": "confirmed", "summary": "<b>🚀 Server calendar smoke</b>",
        "htmlLink": "https://www.google.com/calendar/event?eid=fixture",
        "start": {"dateTime": local(14, 0).to_rfc3339()},
        "end": {"dateTime": local(15, 0).to_rfc3339()},
        "organizer": {"email": "organizer@example.com"}});
    Mock::given(path("/calendar/v3/calendars/primary/events"))
        .and(query_param("syncToken", "baseline"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"items": [event], "nextSyncToken": "after"})),
        )
        .expect(1)
        .mount(&t.server)
        .await;
    Mock::given(path("/calendar/v3/calendars/primary/events"))
        .and(query_param("singleEvents", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
        .mount(&t.server)
        .await;
    Mock::given(path("/gmail/v1/users/me/history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"historyId": "100"})))
        .mount(&t.server)
        .await;
    Mock::given(path("/bottest-token/getUpdates"))
        .and(body_partial_json(json!({"offset": 0})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "result": [text_update(1, 123, "/sync", None)]
        })))
        .expect(1)
        .up_to_n_times(1)
        .mount(&t.server)
        .await;
    Mock::given(path("/bottest-token/getUpdates"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(500))
                .set_body_json(json!({"ok": true, "result": []})),
        )
        .with_priority(10)
        .mount(&t.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/bottest-token/sendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": true, "result": {"message_id": 701}})),
        )
        .mount(&t.server)
        .await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let base = format!("http://127.0.0.1:{port}");
    let api = t.server.uri();
    let mut child = Command::new(env!("CARGO_BIN_EXE_gm"))
        .arg("serve")
        .env_clear()
        .current_dir(std::env::temp_dir())
        .env("GM_ALLOWED_EMAIL", OWNER)
        .env(
            "GM_DATABASE_URL",
            t.app.pool.connect_options().to_url_lossy().as_str(),
        )
        .env("GM_GOOGLE_CLIENT_ID", "client-id")
        .env("GM_SECRET_GOOGLE_CLIENT_SECRET", "client-secret")
        .env("GM_SECRET_MASTER_KEY", &t.app.cfg.master_key)
        .env("GM_SECRET_SESSION_SECRET", &t.app.cfg.session_secret)
        .env("GM_SECRET_METRICS_TOKEN", "test-metrics")
        .env("GM_SECRET_TELEGRAM_BOT_TOKEN", "test-token")
        .env("GM_TELEGRAM_CHAT_ID", "123")
        .env("GM_GOOGLE_AUTH_URL", format!("{api}/auth"))
        .env("GM_GOOGLE_TOKEN_URL", format!("{api}/token"))
        .env("GM_GMAIL_API_BASE", format!("{api}/gmail/v1"))
        .env("GM_CALENDAR_API_BASE", format!("{api}/calendar/v3"))
        .env("GM_TELEGRAM_API_BASE", &api)
        .env("GM_LLM_BASE_URL", format!("{api}/v1"))
        .env("GM_LLM_READY_URL", format!("{api}/ready"))
        .env("GM_DEFAULT_UI_LANGUAGE", "en")
        .env("GM_RULES_PATH", &t.app.cfg.rules_path)
        .env("GM_HOST", "127.0.0.1")
        .env("GM_PORT", port.to_string())
        .env("GM_BASE_URL", &base)
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let http = reqwest::Client::new();
    let outcome = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("gm serve exited: {status}");
            }
            let notified: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM events WHERE source = 'calendar' AND status = 'notified'",
            )
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
            let requests = t.server.received_requests().await.unwrap();
            let sync_replied = requests.iter().any(|r| {
                r.url.path().ends_with("/sendMessage")
                    && r.body_json::<Value>().unwrap()["text"]
                        == "Gmail checked. New emails found: 0."
            });
            let incidents: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM health_state WHERE check_name = 'ingest_lag'",
            )
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
            if notified == 1 && sync_replied && incidents == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            http.get(format!("{base}/healthz"))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        let dashboard = http
            .get(format!("{base}/"))
            .header("Cookie", session_cookie(&t, OWNER))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(dashboard.contains("Server calendar smoke"), "{dashboard}");
        let id: i64 = sqlx::query_scalar("SELECT id FROM events WHERE source = 'calendar'")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
        assert!(!dashboard.contains(&format!("action=\"/events/{id}/eval\"")));
        assert!(!dashboard.contains(&format!("action=\"/events/{id}/feedback\"")));
        let token: String =
            sqlx::query_scalar("SELECT sync_token FROM calendar_state WHERE email = $1")
                .bind(OWNER)
                .fetch_one(&t.app.pool)
                .await
                .unwrap();
        assert_eq!(token, "after");
        let rejected = http
            .post(format!("{base}/events/{id}/eval"))
            .header("Cookie", session_cookie(&t, OWNER))
            .form(&[("expected", "notify")])
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), reqwest::StatusCode::BAD_REQUEST);
        http.post(format!("{base}/events/{id}/feedback"))
            .header("Cookie", session_cookie(&t, OWNER))
            .form(&[("label", "down")])
            .send()
            .await
            .unwrap();
        let feedback: i64 =
            sqlx::query_scalar("SELECT count(*) FROM triage_feedback WHERE event_id = $1")
                .bind(id)
                .fetch_one(&t.app.pool)
                .await
                .unwrap();
        assert_eq!(feedback, 0);

        let requests = t.server.received_requests().await.unwrap();
        let messages: Vec<Value> = requests
            .iter()
            .filter(|r| r.url.path().ends_with("/sendMessage"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(
            messages.len(),
            2,
            "only the calendar notice and requested sync result: {messages:?}"
        );
        let calendar = messages
            .iter()
            .find(|m| {
                m["text"]
                    .as_str()
                    .is_some_and(|s| s.contains("Server calendar smoke"))
            })
            .expect("gm serve must send the calendar notification through Telegram");
        assert!(
            calendar["text"]
                .as_str()
                .unwrap()
                .contains("New invitation")
        );
        assert_eq!(calendar["chat_id"], "123");
        assert!(calendar.get("parse_mode").is_none());
        assert!(
            calendar["text"]
                .as_str()
                .unwrap()
                .contains("<b>🚀 Server calendar smoke</b>")
        );
        assert_eq!(
            calendar["entities"][0],
            json!({"type": "bold", "offset": 3, "length": 14})
        );
        assert_eq!(
            calendar["entities"][1],
            json!({"type": "bold", "offset": 18, "length": 31})
        );
        assert!(
            calendar["entities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entity| entity["type"] == "text_link"
                    && entity["url"] == "https://www.google.com/calendar/event?eid=fixture")
        );
    })
    .await;
    child.kill().await.unwrap();
    let output = child.wait_with_output().await.unwrap();
    assert!(
        outcome.is_ok(),
        "server smoke timed out: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!(
        "gm serve smoke verified: /healthz 200, dashboard Calendar row, Calendar cursor advanced, event notified, Telegram template received"
    );
}
