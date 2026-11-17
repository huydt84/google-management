use gm::i18n::Tr;
use gm::notify::{EntityKind, Notifier, OutgoingMessage, format_email_notification};
use gm::rules::Priority;
use gm::telegram::TelegramClient;
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn formatting_is_localized_and_bounded() {
    let gmail = "https://mail.google.com/x";
    let data = json!({"subject": "件名", "from": "A <a@b.c>", "link": gmail});
    let summary = json!({"summary": "S", "importance": "low", "suggested_actions": [
        "translate",
        {"text": "View run", "url": "https://github.com/o/r/actions/runs/1"}
    ]});
    let ja = format_email_notification(&Tr::new("ja"), &data, Some(&summary), Priority::Normal);
    assert_eq!(
        ja.text,
        "⚪ 📧 件名\n差出人: A <a@b.c>\n\nS\n\n提案:\n• translate\n• View run (github.com)\n\nGmail で開く"
    );
    let span = |text: &str, url: &str| {
        let units: Vec<u16> = ja.text.encode_utf16().collect();
        let e = ja
            .entities
            .iter()
            .find(|e| matches!(&e.kind, EntityKind::TextLink { url: u } if u == url))
            .unwrap_or_else(|| panic!("no link to {url}"));
        assert_eq!(
            String::from_utf16(&units[e.offset..e.offset + e.length]).unwrap(),
            text
        );
    };
    span("View run", "https://github.com/o/r/actions/runs/1");
    span("Gmail で開く", gmail);
    assert_eq!(ja.entities.len(), 2, "unlinked suggestions carry no span");
    // Rule-only (no summary): header, sender and link only.
    let rule_only = format_email_notification(&Tr::new("vi"), &data, None, Priority::Urgent);
    assert_eq!(
        rule_only.text,
        "🚨 📧 件名\nTừ: A <a@b.c>\n\nMở trong Gmail"
    );
    let long = json!({"summary": "x".repeat(5000)});
    let t = format_email_notification(&Tr::new("en"), &data, Some(&long), Priority::Normal);
    assert_eq!(t.text.encode_utf16().count(), gm::notify::TELEGRAM_LIMIT);
    assert!(
        t.entities.is_empty(),
        "the cut removes the Gmail link span with its text"
    );
}

#[tokio::test]
async fn telegram_sends_plain_text_and_silent_flag() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTOKEN/sendMessage"))
        .and(body_partial_json(
            json!({"chat_id": "42", "text": "<b>hi</b>", "disable_notification": true}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;
    let n = TelegramClient::new(reqwest::Client::new(), &server.uri(), "TOKEN", "42");
    n.send(&OutgoingMessage::text("<b>hi</b>", true))
        .await
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert!(body.get("parse_mode").is_none(), "never markup");

    let failing = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&failing)
        .await;
    let n = TelegramClient::new(reqwest::Client::new(), &failing.uri(), "SECRET", "42");
    let err = n
        .send(&OutgoingMessage::text("x", false))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        !err.contains("SECRET"),
        "bot token must not leak into errors: {err}"
    );
}

#[tokio::test]
async fn telegram_returns_ids_edits_and_sends_buttons() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botT/sendMessage"))
        .and(body_partial_json(json!({"reply_markup": {"inline_keyboard": [[{"text": "👍", "callback_data": "f:1:up"}]]}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": {"message_id": 77}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/botT/editMessageText"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({"ok": false, "description": "Bad Request: message is not modified"}),
        ))
        .mount(&server)
        .await;
    let n = TelegramClient::new(reqwest::Client::new(), &server.uri(), "T", "42");
    let msg = OutgoingMessage {
        entities: vec![gm::notify::MessageEntity {
            offset: 0,
            length: 1,
            kind: gm::notify::EntityKind::Bold,
        }],
        text: "x".into(),
        silent: false,
        buttons: vec![vec![gm::notify::Button {
            text: "👍".into(),
            data: "f:1:up".into(),
        }]],
        reply_to: None,
    };
    assert_eq!(n.send(&msg).await.unwrap(), Some(77));
    n.edit(77, &OutgoingMessage::text("x", false))
        .await
        .expect("not modified is fine");
    let bodies: Vec<serde_json::Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        bodies[0]["entities"],
        json!([{"type": "bold", "offset": 0, "length": 1}])
    );
    assert_eq!(
        bodies[1]["entities"],
        json!([]),
        "an edit to plain text clears old formatting"
    );
}

#[test]
fn long_text_is_split_at_lines() {
    let line = "y".repeat(1000);
    let text = [line.as_str(); 9].join("\n");
    let parts = gm::notify::split_message(&text);
    assert_eq!(parts.len(), 3);
    assert!(
        parts
            .iter()
            .all(|p| p.chars().count() <= gm::notify::TELEGRAM_LIMIT)
    );
    assert_eq!(parts.join("\n"), text);
    assert_eq!(gm::notify::split_message("short"), vec!["short"]);
}
