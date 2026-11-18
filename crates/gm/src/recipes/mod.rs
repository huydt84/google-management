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
/// cap) or `accept` turns it down, ask again without thinking (with the usual one retry). The
/// answer without thinking is used whatever `accept` says.
#[allow(clippy::too_many_arguments)]
pub async fn think_json<T>(
    llm: &LlmClient,
    task_name: &str,
    task: &str,
    language: &str,
    block: &str,
    max_tokens: u32,
    parse: impl Fn(&str) -> Option<T>,
    accept: impl Fn(&T) -> bool,
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
    match parse(&first) {
        Some(v) if accept(&v) => {
            return Ok(JsonReply {
                parsed: Some(v),
                raw: first,
            });
        }
        Some(_) => tracing::warn!(
            task = task_name,
            "answer with thinking turned down; asking without"
        ),
        None => tracing::warn!(
            task = task_name,
            "answer with thinking unusable; asking without"
        ),
    }
    llm.chat_json(
        &format!("{task_name}-nothink"),
        Lane::Interactive,
        llm.task_messages(task, language, block),
        max_tokens,
        parse,
    )
    .await
}

/// Whether `text` is written in `language`, for the languages the code can tell by their script or
/// marks: Vietnamese, Japanese, Korean, Chinese and Thai. `None` for any other language (Latin-script
/// languages other than Vietnamese look alike) or a text without letters.
pub fn in_language(language: &str, text: &str) -> Option<bool> {
    let lang = language.to_lowercase();
    let has = |names: &[&str]| names.iter().any(|n| lang.contains(n));
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() {
        return None;
    }
    let share = |f: fn(char) -> bool| {
        letters.iter().filter(|c| f(**c)).count() as f64 / letters.len() as f64
    };
    if has(&["vietnam", "tiếng việt"]) {
        // Most Vietnamese words carry a mark; English (even with Vietnamese names) rarely does.
        let words: Vec<&str> = text
            .split(|c: char| !c.is_alphabetic())
            .filter(|w| !w.is_empty())
            .collect();
        let marked = words
            .iter()
            .filter(|w| w.chars().any(is_vietnamese_mark))
            .count();
        return Some(marked as f64 / words.len() as f64 >= 0.2);
    }
    if has(&["japan", "日本"]) {
        return Some(share(is_kana) > 0.0 && share(|c| is_kana(c) || is_han(c)) >= 0.5);
    }
    if has(&["korea", "한국"]) {
        return Some(
            share(|c| {
                ('\u{AC00}'..='\u{D7AF}').contains(&c) || ('\u{1100}'..='\u{11FF}').contains(&c)
            }) >= 0.5,
        );
    }
    if has(&["chinese", "中文", "汉语", "漢語"]) {
        return Some(share(is_han) >= 0.5);
    }
    if has(&["thai", "ไทย"]) {
        return Some(share(|c| ('\u{0E00}'..='\u{0E7F}').contains(&c)) >= 0.5);
    }
    None
}

fn is_kana(c: char) -> bool {
    ('\u{3040}'..='\u{30FF}').contains(&c) || ('\u{FF66}'..='\u{FF9F}').contains(&c)
}

fn is_han(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c)
}

/// Letters only Vietnamese uses among Latin-script languages: ă â đ ê ô ơ ư and the tone-marked
/// vowels of Latin Extended Additional.
fn is_vietnamese_mark(c: char) -> bool {
    "ăâđêôơưĂÂĐÊÔƠƯ".contains(c) || ('\u{1EA0}'..='\u{1EF9}').contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_is_told_by_script() {
        let en =
            "Email contains a forwarded message from Ngọc Hoàng about resending contract data.";
        let vi =
            "Email chứa thư được chuyển tiếp từ văn phòng bất động sản về việc gửi lại hợp đồng.";
        assert_eq!(in_language("Vietnamese", vi), Some(true));
        assert_eq!(in_language("Vietnamese", en), Some(false));
        assert_eq!(
            in_language("Japanese", "契約書のデータを再送します。"),
            Some(true)
        );
        assert_eq!(in_language("Japanese", en), Some(false));
        assert_eq!(
            in_language("Chinese (Simplified)", "请查看合同数据。"),
            Some(true)
        );
        assert_eq!(
            in_language("Korean", "계약 데이터를 확인하세요."),
            Some(true)
        );
        assert_eq!(in_language("Thai", "โปรดตรวจสอบข้อมูลสัญญา"), Some(true));
        assert_eq!(in_language("Thai", en), Some(false));
        assert_eq!(in_language("French", en), None);
        assert_eq!(in_language("Vietnamese", "123 !!"), None);
    }

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
