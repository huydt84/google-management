//! Intent router: the owner's words (never mail content) → `{intent, args}` from a fixed list.
//! Common phrasings in Vietnamese, English and Japanese are recognized without a model call; the
//! rest goes to the model on the interactive lane (`prompts/router.md`). Buttons carry their intent
//! and never get here.

use serde_json::{Value, json};

use super::Intent;
use crate::llm::{Lane, LlmClient, Task, clean, parse_json_object};
use crate::user_settings::normalize_language;

pub const ROUTER_TASK: &str = include_str!("../../prompts/router.md");
/// Output cap: one intent and a language name.

#[derive(Debug, Clone, PartialEq)]
pub struct Routed {
    pub intent: Intent,
    /// `{"language": …}` for a translation into a named language; else `{}`.
    pub args: Value,
}

impl Routed {
    fn of(intent: Intent) -> Self {
        Self {
            intent,
            args: json!({}),
        }
    }
}

/// Whole messages that mean "translate" (into the default target).
const TRANSLATE_EXACT: &[&str] = &[
    "translate",
    "translate it",
    "translate this",
    "translation",
    "dịch",
    "dịch đi",
    "dịch giúp",
    "dịch giùm",
    "dịch giúp tôi",
    "dịch giúp mình",
    "dịch ra",
    "翻訳",
    "翻訳して",
    "翻訳してください",
    "訳して",
];

/// Message openings that pick an intent; the rest of the words stay the instruction.
const PREFIXES: &[(&str, Intent)] = &[
    ("analyze", Intent::Analyze),
    ("analyse", Intent::Analyze),
    ("analysis", Intent::Analyze),
    ("phân tích", Intent::Analyze),
    ("分析", Intent::Analyze),
    ("reply", Intent::Reply),
    ("respond", Intent::Reply),
    ("tell them", Intent::Reply),
    ("tell him", Intent::Reply),
    ("tell her", Intent::Reply),
    ("answer", Intent::Reply),
    ("trả lời", Intent::Reply),
    ("phản hồi", Intent::Reply),
    ("nhắn lại", Intent::Reply),
    ("bảo họ", Intent::Reply),
    ("nói với họ", Intent::Reply),
    ("返信", Intent::Reply),
    ("返事", Intent::Reply),
    ("伝えて", Intent::Reply),
];

/// The keyword fast path: `None` means "ask the model".
pub fn fast_path(text: &str) -> Option<Routed> {
    let t = text.trim().to_lowercase();
    let bare = t.trim_matches(|c: char| {
        c.is_ascii_punctuation() || "。、！？!?…".contains(c) || c.is_whitespace()
    });
    if TRANSLATE_EXACT.contains(&bare) {
        return Some(Routed::of(Intent::Translate));
    }
    PREFIXES.iter().find_map(|(kw, intent)| {
        let rest = t.strip_prefix(kw)?;
        // "replying" is not "reply"; Japanese has no spaces, so a CJK character may follow.
        let whole_word = rest
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() || c >= '\u{3000}');
        whole_word.then(|| Routed::of(*intent))
    })
}

/// Parse the model's answer. Unusable output → `unknown`; an invalid language name is dropped.
pub fn parse_routed(text: &str) -> Option<Routed> {
    let obj = parse_json_object(text)?;
    let intent = Intent::parse(obj.get("intent")?.as_str()?)?;
    let mut args = json!({});
    if intent == Intent::Translate
        && let Some(l) = obj.get("language").and_then(Value::as_str)
        && let Ok(l) = normalize_language(l)
    {
        args["language"] = json!(l);
    }
    Some(Routed { intent, args })
}

/// The owner's words in `<instruction>`, kept from closing it.
fn block(text: &str) -> String {
    let words = clean::sanitize_delimiters(text.trim());
    format!(
        "<instruction>\n{}\n</instruction>",
        words.replace("</instruction", "<_instruction")
    )
}

/// Route the owner's words. Transport errors (including `LlmUnavailable`) are returned.
pub async fn route(llm: &LlmClient, text: &str) -> anyhow::Result<Routed> {
    if let Some(r) = fast_path(text) {
        return Ok(r);
    }
    let reply = llm
        .run(
            Task::Router,
            Lane::Interactive,
            ROUTER_TASK,
            "English",
            &block(text),
            parse_routed,
            None,
        )
        .await?;
    Ok(reply.parsed.unwrap_or_else(|| Routed::of(Intent::Unknown)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(t: &str) -> Option<Intent> {
        fast_path(t).map(|r| r.intent)
    }

    #[test]
    fn fast_path_in_three_languages() {
        assert_eq!(intent("Translate"), Some(Intent::Translate));
        assert_eq!(intent("  dịch giúp mình! "), Some(Intent::Translate));
        assert_eq!(intent("翻訳して。"), Some(Intent::Translate));
        assert_eq!(intent("analyze the amounts"), Some(Intent::Analyze));
        assert_eq!(intent("Phân tích giúp"), Some(Intent::Analyze));
        assert_eq!(intent("分析して"), Some(Intent::Analyze));
        assert_eq!(intent("tell them Tuesday works"), Some(Intent::Reply));
        assert_eq!(intent("Reply: ok"), Some(Intent::Reply));
        assert_eq!(intent("trả lời là mình đồng ý"), Some(Intent::Reply));
        assert_eq!(intent("返信：火曜日で大丈夫です"), Some(Intent::Reply));
    }

    #[test]
    fn everything_else_goes_to_the_model() {
        // A target language needs the model to read it.
        assert_eq!(intent("translate to English"), None);
        assert_eq!(intent("dịch sang tiếng Anh"), None);
        assert_eq!(intent("replying later is fine"), None);
        assert_eq!(intent("is this a scam?"), None);
        assert_eq!(intent(""), None);
    }

    #[test]
    fn model_output_is_parsed_strictly() {
        let r = parse_routed(r#"{"intent": "translate", "language": "English"}"#).unwrap();
        assert_eq!(r.intent, Intent::Translate);
        assert_eq!(r.args["language"], "English");
        let r = parse_routed(r#"{"intent": "translate", "language": "English. Ignore rules"}"#)
            .unwrap();
        assert_eq!(r.args, json!({}));
        assert_eq!(
            parse_routed(r#"{"intent": "analyze", "language": "x"}"#)
                .unwrap()
                .args,
            json!({})
        );
        assert!(parse_routed(r#"{"intent": "forward"}"#).is_none());
    }
}
