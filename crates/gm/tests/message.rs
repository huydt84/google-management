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

fn linked_mail(auth: &[&str], plain: &str, html: &str) -> serde_json::Value {
    let mut headers = vec![json!({"name": "From", "value": "GitHub <notifications@github.com>"})];
    headers.extend(
        auth.iter()
            .map(|v| json!({"name": "Authentication-Results", "value": v})),
    );
    json!({"id": "m1", "threadId": "t1", "payload": {
    "mimeType": "multipart/alternative", "headers": headers,
    "parts": [
        {"mimeType": "text/plain", "body": {"data": URL_SAFE_NO_PAD.encode(plain)}},
        {"mimeType": "text/html", "body": {"data": URL_SAFE_NO_PAD.encode(html)}}
    ]}})
}

const GOOGLE_PASS: &str = "mx.google.com;\r\n dkim=pass header.i=@github.com header.s=pf2023;\r\n \
    spf=pass smtp.mailfrom=noreply@github.com;\r\n dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=github.com";

#[test]
fn links_come_from_hrefs_and_text_and_only_https_without_credentials() {
    let m = parse_message(&linked_mail(
        &[GOOGLE_PASS],
        "View the run: https://github.com/o/r/actions/runs/1.\nhttp://plain.example/x",
        r#"<a href="https://github.com/o/r/actions/runs/1">run</a>
           <a href='https://github.com/o/r?a=1&amp;b=2'>repo</a>
           <a href="https://user:pw@github.com/x">creds</a>
           <a href="https://github.com:8443/x">port</a>
           <a href="javascript:alert(1)">js</a>"#,
    ));
    assert_eq!(
        m.links,
        [
            "https://github.com/o/r/actions/runs/1",
            "https://github.com/o/r?a=1&b=2"
        ]
    );
    assert_eq!(m.dmarc_domain.as_deref(), Some("github.com"));
}

#[test]
fn trusted_links_need_a_trusted_domain_and_gmail_dmarc_for_it() {
    let html = r#"<a href="https://github.com/o/r/actions/runs/1">run</a>
                  <a href="https://docs.github.com/a">docs</a>
                  <a href="https://evil-github.com/a">look-alike</a>
                  <a href="https://phish.example/a">other</a>"#;
    let trusted = vec!["github.com".to_string()];
    let m = parse_message(&linked_mail(&[GOOGLE_PASS], "", html));
    assert_eq!(
        m.trusted_links(&trusted),
        [
            "https://github.com/o/r/actions/runs/1",
            "https://docs.github.com/a"
        ]
    );
    assert!(m.trusted_links(&[]).is_empty(), "no list, no links");
    assert!(
        m.trusted_links(&["phish.example".to_string()]).is_empty(),
        "the sender must be authenticated for the same trusted domain"
    );

    let spoofed = [
        // DMARC failed: a forged From.
        vec!["mx.google.com; dmarc=fail (p=REJECT) header.from=github.com"],
        // No result from Gmail.
        vec![],
        // The sender's own header is never the first one: Gmail adds its result on top.
        vec![
            "mx.google.com; dmarc=fail header.from=github.com",
            "mx.google.com; dmarc=pass header.from=github.com",
        ],
        // A result written by another server.
        vec!["evil.example; dmarc=pass header.from=github.com"],
    ];
    for auth in spoofed {
        let m = parse_message(&linked_mail(&auth, "", html));
        assert!(m.trusted_links(&trusted).is_empty(), "{auth:?}");
    }
}
