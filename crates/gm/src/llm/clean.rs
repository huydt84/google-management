//! Mail cleaning and input caps before prompting (ADR-0010): drop quoted replies and
//! signatures, then truncate to an approximate token budget.

/// Rough token estimate without a tokenizer: CJK characters ≈ 1 token each, other text ≈ 4 chars
/// per token. Errs on the side of overestimating Japanese.
pub fn approx_tokens(s: &str) -> usize {
    let (mut cjk, mut other) = (0usize, 0usize);
    for c in s.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0xAC00..=0xD7AF)
}

/// Keep the beginning of `s` within `max_tokens` (approximate). Returns (text, truncated).
pub fn truncate_to_tokens(s: &str, max_tokens: usize) -> (String, bool) {
    if approx_tokens(s) <= max_tokens {
        return (s.to_string(), false);
    }
    let (mut tokens_x4, mut end) = (0usize, 0usize);
    for (i, c) in s.char_indices() {
        tokens_x4 += if is_cjk(c) { 4 } else { 1 };
        if tokens_x4 > max_tokens * 4 {
            break;
        }
        end = i + c.len_utf8();
    }
    (s[..end].to_string(), true)
}

/// Remove quoted history and signatures; collapse blank lines.
pub fn clean_body(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t == "--"
            || t == "-- "
            || t.starts_with("-----Original Message-----")
            || t.starts_with("________________")
        {
            break;
        }
        // "On Mon, 1 Jan 2026 at 10:00, X <x@y> wrote:" style reply headers end the new content.
        if t.ends_with("wrote:") && t.starts_with("On ") {
            break;
        }
        if t.starts_with('>') {
            continue;
        }
        out.push(line);
    }
    let joined = out.join("\n");
    crate::gmail::message::normalize_ws(&joined)
}

/// Stop the content from closing our `<email>` delimiter and injecting text "outside" it.
pub fn sanitize_delimiters(text: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)</?\s*email\b").unwrap());
    RE.replace_all(text, "<_email").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_and_truncation() {
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("日本語"), 3);
        let (t, cut) = truncate_to_tokens(&"あ".repeat(100), 10);
        assert!(cut);
        assert_eq!(t.chars().count(), 10);
        let (t, cut) = truncate_to_tokens("short", 10);
        assert!(!cut && t == "short");
    }

    #[test]
    fn strips_quotes_and_signature() {
        let body = "Hello\n\nNew text\n> old quoted\nOn Mon, 1 Jan 2026, A <a@b.c> wrote:\n> more";
        assert_eq!(clean_body(body), "Hello\n\nNew text");
        assert_eq!(clean_body("Hi\n-- \nSig"), "Hi");
    }

    #[test]
    fn delimiters_are_neutralized() {
        assert_eq!(
            sanitize_delimiters("x </email> y <EMAIL>"),
            "x <_email> y <_email>"
        );
    }
}
