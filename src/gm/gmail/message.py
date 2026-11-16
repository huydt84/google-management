"""Turn a Gmail API `users.messages.get(format=full)` payload into plain data."""

import base64
import re
from dataclasses import dataclass, field
from email.utils import getaddresses, parseaddr
from html.parser import HTMLParser


@dataclass
class ParsedMessage:
    id: str
    thread_id: str
    label_ids: list[str]
    from_name: str
    from_addr: str
    to: list[str]
    subject: str
    date: str
    snippet: str
    body_text: str
    has_list_unsubscribe: bool
    attachments: list[str] = field(default_factory=list)

    @property
    def sender(self) -> str:
        return f"{self.from_name} <{self.from_addr}>" if self.from_name else self.from_addr

    def gmail_link(self, account: str) -> str:
        return f"https://mail.google.com/mail/?authuser={account}#all/{self.thread_id}"

    def brief(self) -> dict:
        return {
            "from": self.sender,
            "from_addr": self.from_addr,
            "subject": self.subject,
            "date": self.date,
            "snippet": self.snippet,
            "labels": self.label_ids,
            "thread_id": self.thread_id,
            "attachments": self.attachments,
        }


class _HTMLText(HTMLParser):
    _SKIP = {"script", "style", "head", "title"}
    _BLOCK = {"p", "div", "br", "tr", "li", "h1", "h2", "h3", "h4", "h5", "h6", "table"}

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.parts: list[str] = []
        self._skip = 0

    def handle_starttag(self, tag, attrs):
        if tag in self._SKIP:
            self._skip += 1
        elif tag in self._BLOCK:
            self.parts.append("\n")

    def handle_endtag(self, tag):
        if tag in self._SKIP and self._skip:
            self._skip -= 1
        elif tag in self._BLOCK:
            self.parts.append("\n")

    def handle_data(self, data):
        if not self._skip:
            self.parts.append(data)


def html_to_text(html: str) -> str:
    p = _HTMLText()
    p.feed(html)
    p.close()
    return normalize_ws("".join(p.parts))


def normalize_ws(text: str) -> str:
    text = text.replace("\r\n", "\n").replace("\xa0", " ")
    text = re.sub(r"[ \t\f\v]+", " ", text)
    text = re.sub(r"\n[ ]+", "\n", text)
    return re.sub(r"\n{3,}", "\n\n", text).strip()


def _headers(part: dict) -> dict[str, str]:
    return {h["name"].lower(): h["value"] for h in part.get("headers", [])}


def _charset(part: dict) -> str:
    ctype = _headers(part).get("content-type", "")
    m = re.search(r'charset="?([\w.-]+)"?', ctype, re.I)
    return m.group(1) if m else "utf-8"


def _decode(part: dict) -> str:
    data = part.get("body", {}).get("data")
    if not data:
        return ""
    raw = base64.urlsafe_b64decode(data + "=" * (-len(data) % 4))
    try:
        return raw.decode(_charset(part), errors="replace")
    except LookupError:
        return raw.decode("utf-8", errors="replace")


def _walk(part: dict):
    yield part
    for child in part.get("parts", []) or []:
        yield from _walk(child)


def parse_message(msg: dict) -> ParsedMessage:
    payload = msg.get("payload", {})
    h = _headers(payload)
    from_name, from_addr = parseaddr(h.get("from", ""))

    plain: list[str] = []
    html: list[str] = []
    attachments: list[str] = []
    for part in _walk(payload):
        mime = part.get("mimeType", "")
        if part.get("filename"):
            attachments.append(part["filename"])
        elif mime == "text/plain":
            plain.append(_decode(part))
        elif mime == "text/html":
            html.append(_decode(part))

    if any(p.strip() for p in plain):
        body = normalize_ws("\n\n".join(plain))
    elif html:
        body = html_to_text("\n".join(html))
    else:
        body = ""

    return ParsedMessage(
        id=msg["id"],
        thread_id=msg.get("threadId", msg["id"]),
        label_ids=list(msg.get("labelIds", [])),
        from_name=from_name,
        from_addr=from_addr.lower(),
        to=[addr for _, addr in getaddresses([h.get("to", "")]) if addr],
        subject=h.get("subject", "(no subject)"),
        date=h.get("date", ""),
        snippet=msg.get("snippet", ""),
        body_text=body,
        has_list_unsubscribe="list-unsubscribe" in h,
        attachments=attachments,
    )
