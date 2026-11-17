//! Feedback buttons and the Telegram updates poller (2C + minimal 3A).
mod common;

use common::*;
use serde_json::{Value, json};
use sqlx::PgPool;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, ResponseTemplate};

fn telegram(cfg: &mut gm::config::Config) {
    cfg.telegram_bot_token = Some("T".into());
    cfg.telegram_chat_id = Some("42".into());
}

async fn event(t: &TestApp, rule: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO events (account, source, kind, external_id, status, rule, data)
         VALUES ($1, 'gmail', 'message.received', $2, 'notified', $3, '{\"subject\": \"S\"}') RETURNING id",
    )
    .bind(OWNER)
    .bind(format!("x-{rule}"))
    .bind(rule)
    .fetch_one(&t.app.pool)
    .await
    .unwrap()
}

fn tap(update_id: i64, chat: i64, data: &str, keyboard: Value) -> Value {
    json!({"update_id": update_id, "callback_query": {
        "id": format!("cq{update_id}"), "from": {"id": chat}, "data": data,
        "message": {"message_id": 900, "chat": {"id": chat}, "reply_markup": {"inline_keyboard": keyboard}}}})
}

fn kb(id: i64) -> Value {
    json!([[{"text": "👍", "callback_data": format!("f:{id}:up")}, {"text": "👎", "callback_data": format!("f:{id}:down")}]])
}

async fn label(t: &TestApp, id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT label FROM triage_feedback WHERE event_id = $1")
        .bind(id)
        .fetch_optional(&t.app.pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn notifications_carry_feedback_buttons(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    t.connect_owner().await;
    t.mock_message(gmail_message(
        "f1",
        "Rakuten <no-reply@id.rakuten.co.jp>",
        "[楽天] ログイン通知",
        &["INBOX"],
        "login",
    ))
    .await;
    let id = t.insert_event("f1").await;
    t.drain().await;
    let sent = t.sent();
    let actions = &sent[0].buttons[0];
    assert_eq!(actions[0].data, format!("i:{id}:translate"));
    let row = &sent[0].buttons[1];
    assert_eq!(row[0].data, format!("f:{id}:up"));
    assert_eq!(row[1].data, format!("f:{id}:down"));
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn a_tap_is_recorded_marked_and_the_last_tap_wins(pool: PgPool) {
    let t = test_app(pool, telegram, None).await;
    let id = event(&t, "model").await;
    let poller = t
        .app
        .telegram
        .clone()
        .expect("poller when Telegram is configured");
    poller
        .handle_update(&tap(1, 42, &format!("f:{id}:down"), kb(id)))
        .await
        .unwrap();
    assert_eq!(
        t.notifier.answers.lock().unwrap()[0],
        ("cq1".to_string(), "Thanks, noted.".to_string())
    );
    t.drain().await;
    assert_eq!(label(&t, id).await.as_deref(), Some("down"));
    let edits = t.notifier.button_edits.lock().unwrap().clone();
    assert_eq!(edits[0].0, 900);
    assert_eq!(edits[0].1[0][0].text, "👍");
    assert_eq!(edits[0].1[0][1].text, "✅👎");

    // Change of mind: the other button wins and the marks move.
    let marked = json!([[{"text": "👍", "callback_data": format!("f:{id}:up")}, {"text": "✅👎", "callback_data": format!("f:{id}:down")}]]);
    poller
        .handle_update(&tap(2, 42, &format!("f:{id}:up"), marked))
        .await
        .unwrap();
    t.drain().await;
    assert_eq!(label(&t, id).await.as_deref(), Some("up"));
    let edits = t.notifier.button_edits.lock().unwrap().clone();
    assert_eq!(
        (
            edits[1].1[0][0].text.as_str(),
            edits[1].1[0][1].text.as_str()
        ),
        ("✅👍", "👎")
    );

    // An older tap delivered late doesn't overwrite the newer answer.
    let old = chrono::Utc::now() - chrono::Duration::minutes(5);
    gm::feedback::record(&t.app.pool, id, "down", "telegram", old)
        .await
        .unwrap();
    assert_eq!(label(&t, id).await.as_deref(), Some("up"));
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn presses_from_other_chats_and_unknown_buttons_are_ignored(pool: PgPool) {
    let t = test_app(pool, telegram, None).await;
    let id = event(&t, "model").await;
    let poller = t.app.telegram.clone().unwrap();
    poller
        .handle_update(&tap(1, 666, &format!("f:{id}:down"), kb(id)))
        .await
        .unwrap();
    poller
        .handle_update(&tap(2, 42, "rm -rf", kb(id)))
        .await
        .unwrap();
    poller
        .handle_update(&json!({"update_id": 3, "message": {"text": "hi"}}))
        .await
        .unwrap();
    t.drain().await;
    assert_eq!(label(&t, id).await, None);
    let answers = t.notifier.answers.lock().unwrap().clone();
    assert_eq!(
        answers.len(),
        2,
        "both presses answered so the spinner stops"
    );
    assert_eq!(answers[1].1, "This button is no longer supported.");
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn poller_long_polls_and_keeps_its_offset(pool: PgPool) {
    let t = test_app(pool, telegram, None).await;
    let id = event(&t, "promotions-tab").await;
    Mock::given(method("POST"))
        .and(path("/botT/getUpdates"))
        .and(body_partial_json(
            json!({"offset": 0, "allowed_updates": ["callback_query"]}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"ok": true, "result": [tap(70, 42, &format!("f:{id}:down"), kb(id))]}),
        ))
        .mount(&t.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/botT/getUpdates"))
        .and(body_partial_json(json!({"offset": 71})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []})))
        .mount(&t.server)
        .await;
    let poller = t.app.telegram.clone().unwrap();
    assert_eq!(poller.poll_once().await.unwrap(), 1);
    assert_eq!(poller.poll_once().await.unwrap(), 0, "asks from offset 71");
    t.drain().await;
    let (label, rule, via): (String, String, String) =
        sqlx::query_as("SELECT label, rule, via FROM triage_feedback")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(
        (label.as_str(), rule.as_str(), via.as_str()),
        ("down", "promotions-tab", "telegram")
    );
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn dashboard_feedback_and_hints(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    let rule_ev = event(&t, "newsletters").await;
    let model_ev = event(&t, "model").await;
    let router = gm::web::router(t.app.state.clone());
    let cookie = session_cookie(&t, OWNER);
    for (id, query) in [(rule_ev, ""), (model_ev, "?after=1")] {
        let (status, headers, _) = send(
            &router,
            post_form(
                &format!("/events/{id}/feedback{query}"),
                &cookie,
                "label=down",
            ),
        )
        .await;
        assert_eq!(status, 303);
        assert_eq!(headers["location"], format!("/{query}#event-{id}"));
        let location = headers["location"]
            .to_str()
            .unwrap()
            .split('#')
            .next()
            .unwrap();
        let (_, _, returned) = send(&router, get(location, Some(&cookie))).await;
        assert!(returned.contains(&format!(r#"id="event-{id}""#)));
        assert!(returned.contains("You marked 👎"));
    }
    let (status, _, _) = send(
        &router,
        post_form(&format!("/events/{rule_ev}/feedback"), &cookie, "label=meh"),
    )
    .await;
    assert_eq!(status, 400);
    let (_, _, body) = send(&router, get("/", Some(&cookie))).await;
    assert!(
        body.contains("Rule &#34;newsletters&#34; decided this"),
        "rule hint"
    );
    assert!(body.contains("The model decided this"), "eval hint");
    assert!(
        body.contains(r#"aria-pressed="true">👎</button>"#),
        "the 👎 is shown as pressed"
    );
    assert_eq!(label(&t, model_ev).await.as_deref(), Some("down"));
}
