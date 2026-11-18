//! Whether a model answer is written in the language it was asked for, for the languages the code
//! can tell by script or marks. Used by the language check of [`super::LlmClient::run`].

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
}
