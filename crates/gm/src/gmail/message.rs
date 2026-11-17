//! Turn a Gmail API `users.messages.get(format=full)` payload into plain data. Handles Japanese
//! charsets (ISO-2022-JP, Shift_JIS, EUC-JP) through `encoding_rs`.

use std::sync::LazyLock;

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use regex::Regex;
use serde_json::{Value, json};

#[derive(Debug, Clone, Default)]
pub struct ParsedMessage {
    pub id: String,
    pub thread_id: String,
    pub label_ids: Vec<String>,
    pub from_name: String,
    pub from_addr: String,
    pub to: Vec<String>,
    pub subject: String,
    pub date: String,
    pub snippet: String,
    pub body_text: String,
    pub has_list_unsubscribe: bool,
    pub attachments: Vec<String>,
    /// `Message-ID` header (with angle brackets), used to thread a reply.
    pub message_id_header: String,
    /// `References` header, extended by a reply.
    pub references: String,
    /// `Reply-To` addresses (lowercase); a reply goes here, else to `from_addr`.
    pub reply_to: Vec<String>,
}

impl ParsedMessage {
    pub fn sender(&self) -> String {
        if self.from_name.is_empty() {
            self.from_addr.clone()
        } else {
            format!("{} <{}>", self.from_name, self.from_addr)
        }
    }

    pub fn gmail_link(&self, account: &str) -> String {
        format!(
            "https://mail.google.com/mail/?authuser={account}#all/{}",
            self.thread_id
        )
    }

    /// The metadata stored on the event (no body: bodies are fetched again when needed).
    pub fn brief(&self) -> Value {
        json!({
            "from": self.sender(),
            "from_addr": self.from_addr,
            "from_name": self.from_name,
            "subject": self.subject,
            "date": self.date,
            "snippet": self.snippet,
            "labels": self.label_ids,
            "thread_id": self.thread_id,
            "attachments": self.attachments,
        })
    }
}

/// `"Name" <addr@x>` / `Name <addr@x>` / `addr@x` → (name, lowercase addr).
pub fn parse_address(raw: &str) -> (String, String) {
    let raw = raw.trim();
    if let (Some(lt), Some(gt)) = (raw.rfind('<'), raw.rfind('>'))
        && lt < gt
    {
        let name = raw[..lt].trim().trim_matches('"').trim().to_string();
        return (name, raw[lt + 1..gt].trim().to_lowercase());
    }
    (String::new(), raw.trim_matches('"').to_lowercase())
}

/// Split an address list on commas outside quotes and angle brackets.
pub fn parse_address_list(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut cur, mut quoted, mut angle) = (String::new(), false, false);
    for c in raw.chars() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' if !quoted && !angle => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out.iter()
        .map(|a| parse_address(a).1)
        .filter(|a| !a.is_empty())
        .collect()
}

fn headers(part: &Value) -> Vec<(String, String)> {
    part["headers"]
        .as_array()
        .map(|hs| {
            hs.iter()
                .map(|h| {
                    (
                        h["name"].as_str().unwrap_or_default().to_lowercase(),
                        h["value"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn header<'a>(hs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    hs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

static CHARSET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)charset\s*=\s*"?([\w.:-]+)"?"#).unwrap());

fn decode_part(part: &Value) -> String {
    let Some(data) = part["body"]["data"].as_str() else {
        return String::new();
    };
    let raw = URL_SAFE_NO_PAD
        .decode(data.trim_end_matches('='))
        .or_else(|_| URL_SAFE.decode(data))
        .unwrap_or_default();
    let hs = headers(part);
    let label = header(&hs, "content-type")
        .and_then(|ct| CHARSET.captures(ct).map(|c| c[1].to_string()))
        .unwrap_or_else(|| "utf-8".into());
    let enc = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    enc.decode(&raw).0.into_owned()
}

fn walk<'a>(part: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(part);
    if let Some(children) = part["parts"].as_array() {
        for c in children {
            walk(c, out);
        }
    }
}

static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\x0B\x0C]+").unwrap());
static NL_SPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n +").unwrap());
static MANY_NL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

pub fn normalize_ws(text: &str) -> String {
    let t = text.replace("\r\n", "\n").replace('\u{a0}', " ");
    let t = WS.replace_all(&t, " ");
    let t = NL_SPACE.replace_all(&t, "\n");
    MANY_NL.replace_all(&t, "\n\n").trim().to_string()
}

/// Minimal HTML → text: drops script/style/head/title, turns block tags into newlines, decodes
/// entities. Good enough for summarization input; never rendered as HTML.
pub fn html_to_text(html: &str) -> String {
    const SKIP: &[&str] = &["script", "style", "head", "title"];
    const BLOCK: &[&str] = &[
        "p", "div", "br", "tr", "li", "h1", "h2", "h3", "h4", "h5", "h6", "table",
    ];
    let mut out = String::with_capacity(html.len() / 2);
    let mut skip_depth = 0usize;
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        if skip_depth == 0 {
            out.push_str(&rest[..lt]);
        }
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else {
            rest = "";
            break;
        };
        let tag = &after[..gt];
        rest = &after[gt + 1..];
        if tag.starts_with('!') {
            continue; // comment / doctype
        }
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if SKIP.contains(&name.as_str()) {
            if closing {
                skip_depth = skip_depth.saturating_sub(1);
            } else if !tag.ends_with('/') {
                skip_depth += 1;
            }
        } else if BLOCK.contains(&name.as_str()) && skip_depth == 0 {
            out.push('\n');
        }
    }
    if skip_depth == 0 {
        out.push_str(rest);
    }
    normalize_ws(&html_escape::decode_html_entities(&out))
}

pub fn parse_message(msg: &Value) -> ParsedMessage {
    let payload = &msg["payload"];
    let hs = headers(payload);
    let (from_name, from_addr) = parse_address(header(&hs, "from").unwrap_or_default());

    let mut parts = Vec::new();
    walk(payload, &mut parts);
    let (mut plain, mut html, mut attachments) = (Vec::new(), Vec::new(), Vec::new());
    for p in parts {
        let mime = p["mimeType"].as_str().unwrap_or_default();
        match p["filename"].as_str() {
            Some(f) if !f.is_empty() => attachments.push(f.to_string()),
            _ if mime == "text/plain" => plain.push(decode_part(p)),
            _ if mime == "text/html" => html.push(decode_part(p)),
            _ => {}
        }
    }
    let body_text = if plain.iter().any(|p| !p.trim().is_empty()) {
        normalize_ws(&plain.join("\n\n"))
    } else if !html.is_empty() {
        html_to_text(&html.join("\n"))
    } else {
        String::new()
    };

    let id = msg["id"].as_str().unwrap_or_default().to_string();
    ParsedMessage {
        thread_id: msg["threadId"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| id.clone()),
        id,
        label_ids: msg["labelIds"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        from_name,
        from_addr,
        to: parse_address_list(header(&hs, "to").unwrap_or_default()),
        subject: header(&hs, "subject").unwrap_or("(no subject)").to_string(),
        date: header(&hs, "date").unwrap_or_default().to_string(),
        snippet: msg["snippet"].as_str().unwrap_or_default().to_string(),
        body_text,
        has_list_unsubscribe: header(&hs, "list-unsubscribe").is_some(),
        attachments,
        message_id_header: header(&hs, "message-id")
            .unwrap_or_default()
            .trim()
            .to_string(),
        references: header(&hs, "references")
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        reply_to: parse_address_list(header(&hs, "reply-to").unwrap_or_default()),
    }
}
