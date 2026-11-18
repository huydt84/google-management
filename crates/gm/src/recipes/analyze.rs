//! Analyze: key points, deadlines, amounts, phishing signs and next steps, in the owner's summary
//! language, with the model's thinking on. The owner's words (if any) say what to focus on.

use async_trait::async_trait;
use serde_json::Value;

use super::{in_language, owner_words, think_json};
use crate::i18n::Tr;
use crate::llm::{parse_json_object, strip_think};
use crate::tasks::{Intent, Recipe, RecipeContext, RecipeResult};

pub const ANALYZE_TASK: &str = include_str!("../../prompts/analyze.md");
/// Room for the reasoning and the answer.
pub const MAX_OUTPUT_TOKENS: u32 = 2048;

const SECTIONS: &[&str] = &["key_points", "deadlines", "amounts", "risks", "next_steps"];

/// Told to the model before the mail when [`ParsedMessage::is_forward`] holds (`prompts/analyze.md`
/// says how to judge it).
///
/// [`ParsedMessage::is_forward`]: crate::gmail::message::ParsedMessage::is_forward
pub const FORWARDED: &str = "Forwarded: yes. The sender in \"From\" forwarded this mail; the \
original sender is in the forwarded part.\n\n";

/// Every item of every section, for the language check.
pub fn analysis_text(o: &serde_json::Map<String, Value>) -> String {
    SECTIONS
        .iter()
        .filter_map(|k| o.get(*k).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The model's JSON, if it has at least one known section or a verdict.
pub fn parse_analysis(text: &str) -> Option<serde_json::Map<String, Value>> {
    let o = parse_json_object(text)?;
    (SECTIONS
        .iter()
        .any(|k| o.get(*k).is_some_and(Value::is_array))
        || o.contains_key("verdict"))
    .then_some(o)
}

/// Plain text with localized headings; empty sections are left out.
pub fn format_analysis(tr: &Tr, subject: &str, o: &serde_json::Map<String, Value>) -> String {
    let mut lines = vec![format!("🔎 {subject}")];
    if let Some(v) = o
        .get("verdict")
        .and_then(Value::as_str)
        .filter(|v| ["safe", "suspicious", "phishing"].contains(v))
    {
        lines.push(tr.get(&format!("analyze-verdict-{v}")));
    }
    for key in SECTIONS {
        let items: Vec<&str> = o
            .get(*key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .take(5)
                    .collect()
            })
            .unwrap_or_default();
        if items.is_empty() {
            continue;
        }
        lines.push(String::new());
        lines.push(tr.get(&format!("analyze-{}", key.replace('_', "-"))));
        lines.extend(items.into_iter().map(|i| format!("• {i}")));
    }
    lines.join("\n")
}

pub struct Analyze;

#[async_trait]
impl Recipe for Analyze {
    fn intent(&self) -> Intent {
        Intent::Analyze
    }

    async fn run(&self, cx: RecipeContext<'_>) -> anyhow::Result<RecipeResult> {
        let tr = cx.tr();
        let m = cx.message().await?;
        let mut block = String::new();
        if let Some(focus) = cx.instruction.filter(|t| !t.trim().is_empty()) {
            block.push_str(&format!(
                "Owner's question (trusted): {}\n\n",
                owner_words(focus)
            ));
        }
        if m.is_forward() {
            // A fact from code only: Google content stays inside `<email>`.
            block.push_str(FORWARDED);
        }
        block.push_str(&cx.llm.email_block(&m, usize::MAX));
        let language = &cx.settings.summary_language;
        let reply = think_json(
            cx.llm,
            "analyze",
            ANALYZE_TASK,
            language,
            &block,
            MAX_OUTPUT_TOKENS,
            parse_analysis,
            // The thinking model can drift into English: retry without thinking.
            |o| in_language(language, &analysis_text(o)) != Some(false),
        )
        .await?;
        let text = match reply.parsed {
            Some(o) => format_analysis(&tr, &m.subject, &o),
            None => {
                let raw = strip_think(&reply.raw);
                anyhow::ensure!(!raw.is_empty(), "empty analysis from the model");
                format!("🔎 {}\n\n{raw}", m.subject)
            }
        };
        Ok(RecipeResult::Text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analysis_is_formatted_with_headings() {
        let o = parse_analysis(
            r#"{"key_points": ["Invoice for March"], "deadlines": [], "amounts": ["¥5,000 due"],
                "risks": ["Link domain differs from sender"], "next_steps": ["Check in the app"],
                "verdict": "suspicious"}"#,
        )
        .unwrap();
        let text = format_analysis(&Tr::new("en"), "Invoice", &o);
        assert!(text.starts_with("🔎 Invoice\n"));
        assert!(text.contains("• ¥5,000 due"));
        assert!(!text.contains(&Tr::new("en").get("analyze-deadlines")));
        assert!(parse_analysis(r#"{"summary": "x"}"#).is_none());
    }
}
