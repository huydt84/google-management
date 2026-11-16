use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use gm::gmail::message::{html_to_text, parse_address, parse_address_list, parse_message};
use serde_json::json;

#[test]
fn addresses() {
    assert_eq!(
        parse_address("\"楽天カード株式会社\" <Info@Mail.Rakuten-Card.co.jp>"),
        (
            "楽天カード株式会社".into(),
            "info@mail.rakuten-card.co.jp".into()
        )
    );
    assert_eq!(parse_address("a@b.c"), (String::new(), "a@b.c".into()));
    assert_eq!(
        parse_address_list("\"Doe, J\" <j@x.y>, k@x.y"),
        vec!["j@x.y", "k@x.y"]
    );
}

#[test]
fn html_to_text_drops_scripts_and_decodes_entities() {
    let t = html_to_text(
        "<html><head><title>x</title><style>p{}</style></head><body><p>Hello&nbsp;&amp; bye</p><script>alert(1)</script><div>Next</div></body></html>",
    );
    assert_eq!(t, "Hello & bye\n\nNext");
}

#[test]
fn multipart_prefers_plain_and_lists_attachments() {
    let msg = json!({
        "id": "m1", "threadId": "t1", "labelIds": ["INBOX"], "snippet": "s",
        "payload": {
            "mimeType": "multipart/mixed",
            "headers": [{"name": "From", "value": "A <a@b.c>"}, {"name": "Subject", "value": "Hi"},
                        {"name": "List-Unsubscribe", "value": "<mailto:u@b.c>"}, {"name": "To", "value": "x@y.z, w@y.z"}],
            "parts": [
                {"mimeType": "multipart/alternative", "parts": [
                    {"mimeType": "text/plain", "body": {"data": URL_SAFE_NO_PAD.encode("plain body")}},
                    {"mimeType": "text/html", "body": {"data": URL_SAFE_NO_PAD.encode("<p>html body</p>")}}
                ]},
                {"mimeType": "application/pdf", "filename": "invoice.pdf", "body": {"attachmentId": "x"}}
            ]
        }
    });
    let m = parse_message(&msg);
    assert_eq!(m.body_text, "plain body");
    assert_eq!(m.attachments, vec!["invoice.pdf"]);
    assert!(m.has_list_unsubscribe);
    assert_eq!(m.to, vec!["x@y.z", "w@y.z"]);
    assert_eq!(m.sender(), "A <a@b.c>");
}

#[test]
fn japanese_charsets_are_decoded() {
    let (bytes, _, _) = encoding_rs::ISO_2022_JP.encode("ご利用金額 1,234 円");
    let msg = json!({
        "id": "m2",
        "payload": {
            "mimeType": "text/plain",
            "headers": [{"name": "Content-Type", "value": "text/plain; charset=\"ISO-2022-JP\""}],
            "body": {"data": URL_SAFE_NO_PAD.encode(&bytes)}
        }
    });
    let m = parse_message(&msg);
    assert_eq!(m.body_text, "ご利用金額 1,234 円");
    assert_eq!(m.thread_id, "m2");
    assert_eq!(m.subject, "(no subject)");
}

#[test]
fn html_only_falls_back_to_text() {
    let msg = json!({"id": "m3", "payload": {"mimeType": "text/html", "body": {"data": URL_SAFE_NO_PAD.encode("<div>A</div><br>B")}}});
    assert_eq!(parse_message(&msg).body_text, "A\n\nB");
}
