//! Build the RFC 5322 reply that becomes a Gmail draft. Every header value comes from the original
//! mail's headers or from code, never from model output (invariant 2); only the body is written by
//! the model. Header values are stripped of line breaks (no header injection) and non-ASCII text is
//! RFC 2047 encoded; the body is UTF-8, base64.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

use crate::gmail::message::{ParsedMessage, normalize_ws};

#[derive(Debug, Clone, PartialEq)]
pub struct ReplyDraft {
    pub to: Vec<String>,
    pub subject: String,
    pub in_reply_to: String,
    pub references: String,
    pub thread_id: String,
    pub body: String,
}

/// A reply to `m`: to its `Reply-To` (else `From`), `Re:` subject, threaded on its `Message-ID`.
pub fn reply_to(m: &ParsedMessage, body: &str) -> ReplyDraft {
    let to: Vec<String> = if m.reply_to.is_empty() {
        vec![m.from_addr.clone()]
    } else {
        m.reply_to.clone()
    };
    let references = format!("{} {}", m.references, m.message_id_header)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    ReplyDraft {
        to: to.into_iter().filter(|a| is_address(a)).collect(),
        subject: reply_subject(&m.subject),
        in_reply_to: m.message_id_header.clone(),
        references,
        thread_id: m.thread_id.clone(),
        body: body.trim().to_string(),
    }
}

/// A bare `local@domain` address with nothing that could break a header.
pub fn is_address(a: &str) -> bool {
    let ok_chars = a
        .chars()
        .all(|c| !c.is_whitespace() && !c.is_control() && !"<>,;\"()[]\\".contains(c));
    let mut parts = a.split('@');
    ok_chars
        && matches!((parts.next(), parts.next(), parts.next()), (Some(l), Some(d), None) if !l.is_empty() && d.contains('.'))
}

pub fn reply_subject(subject: &str) -> String {
    let s = one_line(subject);
    let lower = s.to_lowercase();
    if lower.starts_with("re:") || lower.starts_with("re：") {
        s
    } else {
        format!("Re: {s}")
    }
}

fn one_line(v: &str) -> String {
    v.replace(['\r', '\n'], " ").trim().to_string()
}

/// RFC 2047 `=?UTF-8?B?…?=` words for non-ASCII values, each under the 75-character limit.
pub fn encode_header(v: &str) -> String {
    let v = one_line(v);
    if v.is_ascii() {
        return v;
    }
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in v.chars() {
        if chunk.len() + c.len_utf8() > 39 {
            words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(&chunk)));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(&chunk)));
    }
    words.join("\r\n ")
}

/// Undo [`encode_header`] (Gmail may return header values as written).
pub fn decode_header(v: &str) -> String {
    if !v.contains("=?") {
        return v.to_string();
    }
    let mut out = String::new();
    for word in v.split_whitespace() {
        let decoded = word
            .strip_prefix("=?UTF-8?B?")
            .or_else(|| word.strip_prefix("=?utf-8?B?"))
            .or_else(|| word.strip_prefix("=?utf-8?b?"))
            .and_then(|w| w.strip_suffix("?="))
            .and_then(|b| STANDARD.decode(b).ok())
            .and_then(|b| String::from_utf8(b).ok());
        match decoded {
            Some(d) => out.push_str(&d),
            None => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(word);
            }
        }
    }
    out
}

impl ReplyDraft {
    /// The message as RFC 5322 text (CRLF line ends).
    pub fn rfc822(&self) -> String {
        let mut h = vec![
            format!("To: {}", self.to.join(", ")),
            format!("Subject: {}", encode_header(&self.subject)),
        ];
        if !self.in_reply_to.is_empty() {
            h.push(format!("In-Reply-To: {}", one_line(&self.in_reply_to)));
        }
        if !self.references.is_empty() {
            h.push(format!("References: {}", one_line(&self.references)));
        }
        h.push("MIME-Version: 1.0".into());
        h.push("Content-Type: text/plain; charset=UTF-8".into());
        h.push("Content-Transfer-Encoding: base64".into());
        let body = STANDARD.encode(self.body.replace("\r\n", "\n").replace('\n', "\r\n"));
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(76)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect();
        format!("{}\r\n\r\n{}\r\n", h.join("\r\n"), lines.join("\r\n"))
    }

    /// base64url, as `drafts.create` wants it.
    pub fn raw(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.rfc822())
    }
}

/// Hash of a body as Gmail will give it back (whitespace normalized like the parser does).
pub fn body_hash(body: &str) -> String {
    crate::tasks::sha256_hex(normalize_ws(body).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn original() -> ParsedMessage {
        ParsedMessage {
            id: "m1".into(),
            thread_id: "t1".into(),
            from_addr: "alice@example.com".into(),
            subject: "Meeting\r\nBcc: x@evil.example".into(),
            message_id_header: "<abc@mail.example.com>".into(),
            references: "<root@mail.example.com>".into(),
            ..Default::default()
        }
    }

    #[test]
    fn reply_headers_come_from_the_original() {
        let d = reply_to(&original(), "Tuesday works.\n");
        assert_eq!(d.to, vec!["alice@example.com"]);
        assert_eq!(d.subject, "Re: Meeting  Bcc: x@evil.example");
        assert_eq!(d.in_reply_to, "<abc@mail.example.com>");
        assert_eq!(
            d.references,
            "<root@mail.example.com> <abc@mail.example.com>"
        );
        let text = d.rfc822();
        assert!(!text.contains("\r\nBcc:"), "no header injection: {text}");
        assert!(text.contains("In-Reply-To: <abc@mail.example.com>\r\n"));
        let mut m = original();
        m.reply_to = vec!["desk@example.com".into(), "bad address".into()];
        assert_eq!(reply_to(&m, "x").to, vec!["desk@example.com"]);
        assert_eq!(reply_subject("RE: x"), "RE: x");
    }

    #[test]
    fn non_ascii_subject_and_body_round_trip() {
        let mut m = original();
        m.subject = "会議の件について、来週の火曜日でよろしいでしょうか".into();
        let d = reply_to(&m, "火曜日で大丈夫です。\nよろしくお願いします。");
        let text = d.rfc822();
        let subject_line = text.lines().find(|l| l.starts_with("Subject:")).unwrap();
        assert!(subject_line.is_ascii() && subject_line.len() <= 78);
        let folded: String = text.split("\r\n\r\n").next().unwrap().replace("\r\n ", " ");
        let subject = folded
            .lines()
            .find_map(|l| l.strip_prefix("Subject: "))
            .unwrap();
        assert_eq!(decode_header(subject), format!("Re: {}", m.subject));
        let body = text.split("\r\n\r\n").nth(1).unwrap().replace("\r\n", "");
        let decoded = String::from_utf8(STANDARD.decode(body).unwrap()).unwrap();
        assert_eq!(decoded, "火曜日で大丈夫です。\r\nよろしくお願いします。");
        assert_eq!(
            body_hash("火曜日で大丈夫です。\r\nよろしくお願いします。"),
            body_hash("火曜日で大丈夫です。\nよろしくお願いします。\n")
        );
    }
}
