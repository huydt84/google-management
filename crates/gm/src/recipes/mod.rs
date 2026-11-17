//! Recipes (workstream 3C): one per intent, written in code ([ADR-0003](../../../docs/adr/0003-intent-router-before-agent.md)).
//! Each re-fetches the mail and shows it to the model only inside `<email>`; the model writes
//! content (a translation, an analysis, a reply body) and nothing else. Calls use the interactive
//! lane.

pub mod analyze;
pub mod reply;
pub mod translate;

use crate::llm::{JsonReply, Lane, LlmClient, clean, parse_json_object, strip_think};

/// The owner's words for a prompt: trusted, but kept from closing the `<email>` block.
pub fn owner_words(text: &str) -> String {
    clean::sanitize_delimiters(text.trim())
}

/// The string under `key` in the model's JSON answer. Long answers can be cut off by the token cap
/// and leave the JSON unterminated; then the text after `"key": "` is kept, unescaped.
pub fn text_of(raw: &str, key: &str) -> Option<String> {
    if let Some(o) = parse_json_object(raw) {
        return o
            .get(key)?
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    let t = strip_think(raw);
    let rest = t[t.find(&format!("\"{key}\""))? + key.len() + 2..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    let rest = rest
        .trim_end()
        .trim_end_matches('}')
        .trim_end()
        .trim_end_matches("```")
        .trim_end()
        .trim_end_matches('"');
    let unescaped = serde_json::from_str::<String>(&format!("\"{rest}\"")).unwrap_or_else(|_| {
        rest.replace("\\n", "\n")
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    });
    Some(unescaped.trim().to_string()).filter(|s| !s.is_empty())
}

/// Ask with the model's thinking on; if that answer is unusable (the reasoning can use up the token
/// cap), ask again without thinking (with the usual one retry).
pub async fn think_json<T>(
    llm: &LlmClient,
    task_name: &str,
    task: &str,
    language: &str,
    block: &str,
    max_tokens: u32,
    parse: impl Fn(&str) -> Option<T>,
) -> anyhow::Result<JsonReply<T>> {
    let first = llm
        .chat(
            task_name,
            Lane::Interactive,
            &llm.task_messages_with(task, language, block, true),
            max_tokens,
            0.6,
        )
        .await?;
    if let Some(v) = parse(&first) {
        return Ok(JsonReply {
            parsed: Some(v),
            raw: first,
        });
    }
    tracing::warn!(
        task = task_name,
        "answer with thinking unusable; asking without"
    );
    llm.chat_json(
        &format!("{task_name}-nothink"),
        Lane::Interactive,
        llm.task_messages(task, language, block),
        max_tokens,
        parse,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_salvaged_from_cut_off_json() {
        assert_eq!(
            text_of(r#"<think>x</think>{"body": "Hi,\nok"}"#, "body").as_deref(),
            Some("Hi,\nok")
        );
        assert_eq!(
            text_of("{\"translation\": \"Xin chào,\\nhẹn gặp", "translation").as_deref(),
            Some("Xin chào,\nhẹn gặp")
        );
        assert_eq!(text_of(r#"{"body": ""}"#, "body"), None);
        assert_eq!(text_of("no json here", "body"), None);
        assert_eq!(text_of("<think>never finished", "body"), None);
    }
}
