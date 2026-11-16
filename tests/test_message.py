import base64

from conftest import gmail_message

from gm.gmail.message import html_to_text, parse_message


def _b64bytes(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).decode()


def test_plain_text_preferred_and_charset_respected():
    msg = {
        "id": "m1", "threadId": "t1", "labelIds": ["INBOX"], "snippet": "s",
        "payload": {
            "mimeType": "multipart/mixed",
            "headers": [{"name": "From", "value": '"Café Bot" <BOT@Example.com>'},
                        {"name": "Subject", "value": "Hóa đơn"},
                        {"name": "List-Unsubscribe", "value": "<mailto:u@x>"}],
            "parts": [
                {"mimeType": "multipart/alternative", "parts": [
                    {"mimeType": "text/plain",
                     "headers": [{"name": "Content-Type",
                                  "value": "text/plain; charset=iso-8859-1"}],
                     "body": {"data": _b64bytes("Prix: 10€ caf\xe9".encode("latin-1", "replace"))}},
                    {"mimeType": "text/html", "body": {"data": _b64bytes(b"<p>ignored</p>")}},
                ]},
                {"mimeType": "application/pdf", "filename": "invoice.pdf", "body": {}},
            ],
        },
    }
    m = parse_message(msg)
    assert m.from_name == "Café Bot" and m.from_addr == "bot@example.com"
    assert "café" in m.body_text and "ignored" not in m.body_text
    assert m.attachments == ["invoice.pdf"]
    assert m.has_list_unsubscribe
    assert m.subject == "Hóa đơn"


def test_html_only_strips_scripts_and_styles():
    text = html_to_text("<html><head><style>x{}</style></head><body><p>Xin&nbsp;chào</p>"
                        "<script>alert(1)</script><div>Dòng 2</div></body></html>")
    assert text == "Xin chào\n\nDòng 2"


def test_simple_message_brief():
    m = parse_message(gmail_message("abc", subject="Hi"))
    assert m.thread_id == "t-abc"
    assert m.brief()["subject"] == "Hi"
    assert "#all/t-abc" in m.gmail_link("owner@example.com")
