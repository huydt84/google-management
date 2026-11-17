//! Translate: the whole mail (subject and body without quoted history or signature) into the
//! owner's translation language, or the language the owner named. Long mail goes in chunks.
//! `/no_think`.

use async_trait::async_trait;

use super::text_of;
use crate::llm::{Lane, clean, strip_think};
use crate::tasks::{Intent, Recipe, RecipeContext, RecipeResult};

pub const TRANSLATE_TASK: &str = include_str!("../../prompts/translate.md");
/// Input per model call (approximate tokens).
pub const CHUNK_TOKENS: usize = 1200;
/// At most this many chunks; the rest is cut and the owner is told.
pub const MAX_CHUNKS: usize = 6;
pub const MAX_OUTPUT_TOKENS: u32 = 2048;

/// Split `Subject` + body into chunks of about [`CHUNK_TOKENS`] at paragraph boundaries. Returns
/// the chunks and whether text beyond [`MAX_CHUNKS`] was dropped.
pub fn chunks(subject: &str, body: &str) -> (Vec<String>, bool) {
    let text = format!("Subject: {subject}\n\n{body}");
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut pieces: Vec<String> = Vec::new();
    for para in text.split("\n\n") {
        // A paragraph longer than a chunk is cut into chunk-sized pieces.
        let mut rest = para.to_string();
        loop {
            let (head, cut) = clean::truncate_to_tokens(&rest, CHUNK_TOKENS);
            pieces.push(head.clone());
            if !cut {
                break;
            }
            rest = rest[head.len()..].to_string();
        }
    }
    for p in pieces {
        if !cur.is_empty() && clean::approx_tokens(&cur) + clean::approx_tokens(&p) > CHUNK_TOKENS {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(&p);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    let truncated = out.len() > MAX_CHUNKS;
    out.truncate(MAX_CHUNKS);
    (out, truncated)
}

pub struct Translate;

#[async_trait]
impl Recipe for Translate {
    fn intent(&self) -> Intent {
        Intent::Translate
    }

    async fn run(&self, cx: RecipeContext<'_>) -> anyhow::Result<RecipeResult> {
        let tr = cx.tr();
        let m = cx.message().await?;
        let language = cx.args["language"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| cx.settings.effective_translation_language().to_string());
        let body = clean::clean_body(if m.body_text.is_empty() {
            &m.snippet
        } else {
            &m.body_text
        });
        let (parts, truncated) = chunks(&m.subject, &body);
        let mut out = Vec::new();
        for part in &parts {
            let block = format!("<email>\n{}\n</email>", clean::sanitize_delimiters(part));
            let reply = cx
                .llm
                .chat_json(
                    "translate",
                    Lane::Interactive,
                    cx.llm.task_messages(TRANSLATE_TASK, &language, &block),
                    MAX_OUTPUT_TOKENS,
                    |t| text_of(t, "translation"),
                )
                .await?;
            out.push(reply.parsed.unwrap_or_else(|| strip_think(&reply.raw)));
        }
        let mut text = format!(
            "{}\n\n{}",
            tr.args("translate-header", &[("language", language.clone())]),
            out.join("\n\n")
        );
        if truncated {
            text.push_str(&format!("\n\n{}", tr.get("translate-truncated")));
        }
        Ok(RecipeResult::Text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_mail_is_chunked_at_paragraphs() {
        let para = "word ".repeat(400); // ~500 tokens
        let body = vec![para.clone(); 5].join("\n\n");
        let (parts, cut) = chunks("Hello", &body);
        assert!(!cut);
        assert!(parts[0].starts_with("Subject: Hello\n\n"));
        assert!(parts.len() >= 2);
        assert!(
            parts
                .iter()
                .all(|p| clean::approx_tokens(p) <= CHUNK_TOKENS + 10)
        );
        let huge = "x".repeat(4 * CHUNK_TOKENS * (MAX_CHUNKS + 2));
        let (parts, cut) = chunks("S", &huge);
        assert!(cut);
        assert_eq!(parts.len(), MAX_CHUNKS);
    }
}
