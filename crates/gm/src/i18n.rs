//! UI and bot strings in vi / en / ja (ADR-0014). Strings live in `locales/<lang>/main.ftl`;
//! missing keys fall back to English.

use std::borrow::Cow;
use std::collections::HashMap;

use fluent_templates::fluent_bundle::FluentValue;
use fluent_templates::{Loader, static_loader};
use unic_langid::LanguageIdentifier;

static_loader! {
    static LOCALES = {
        locales: "./locales",
        fallback_language: "en-US",
        customise: |bundle| bundle.set_use_isolating(false),
    };
}

/// Interface languages offered in Settings: (code stored in settings, native name).
pub const UI_LANGUAGES: &[(&str, &str)] =
    &[("en", "English"), ("vi", "Tiếng Việt"), ("ja", "日本語")];

pub fn is_ui_language(code: &str) -> bool {
    UI_LANGUAGES.iter().any(|(c, _)| *c == code)
}

#[derive(Clone, Debug)]
pub struct Tr {
    lang: LanguageIdentifier,
    pub code: String,
}

impl Tr {
    pub fn new(code: &str) -> Self {
        let code = if is_ui_language(code) { code } else { "en" };
        let id = match code {
            "vi" => "vi",
            "ja" => "ja",
            _ => "en-US",
        };
        Self {
            lang: id.parse().expect("valid language id"),
            code: code.to_string(),
        }
    }

    pub fn get(&self, key: &str) -> String {
        LOCALES.lookup(&self.lang, key)
    }

    pub fn args(&self, key: &str, args: &[(&'static str, String)]) -> String {
        let map: HashMap<Cow<'static, str>, FluentValue> = args
            .iter()
            .map(|(k, v)| (Cow::Borrowed(*k), FluentValue::from(v.clone())))
            .collect();
        LOCALES.lookup_with_args(&self.lang, key, &map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(lang: &str) -> Vec<String> {
        let path = format!("{}/locales/{lang}/main.ftl", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|l| l.split_once(" = ").map(|(k, _)| k.trim().to_string()))
            .collect()
    }

    #[test]
    fn every_locale_has_every_english_key() {
        let en = keys("en-US");
        for lang in ["vi", "ja"] {
            let other = keys(lang);
            let missing: Vec<_> = en.iter().filter(|k| !other.contains(k)).collect();
            assert!(missing.is_empty(), "{lang} is missing {missing:?}");
        }
    }

    #[test]
    fn lookup_and_args() {
        assert_eq!(Tr::new("ja").get("save"), "保存");
        assert_eq!(Tr::new("xx").get("save"), "Save");
        assert!(
            Tr::new("vi")
                .args("ingest-poll", &[("seconds", "42".into())])
                .contains("42")
        );
    }
}
