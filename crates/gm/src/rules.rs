//! Rules engine v2 (ADR-0012). `rules.yaml`: the first matching rule wins; every condition inside
//! one `when` must match. Reloaded automatically when the file changes. Each rule may carry
//! `examples` (redacted headers) that must land on that rule; `gm rules check` and the test suite
//! verify them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::Context;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::gmail::message::{ParsedMessage, parse_address};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Ignore,
    Digest,
    Notify,
    #[default]
    Triage,
}

impl Action {
    /// The three outcomes a mail can end with (rules, model triage, eval cases); `triage` is only a
    /// routing step.
    pub const FINAL: [Action; 3] = [Action::Notify, Action::Digest, Action::Ignore];

    pub fn as_str(&self) -> &'static str {
        match self {
            Action::Ignore => "ignore",
            Action::Digest => "digest",
            Action::Notify => "notify",
            Action::Triage => "triage",
        }
    }

    /// One of [`Action::FINAL`] by name (case-insensitive, trimmed).
    pub fn parse_final(s: &str) -> Option<Action> {
        let s = s.trim();
        Self::FINAL
            .into_iter()
            .find(|a| a.as_str().eq_ignore_ascii_case(s))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Silent,
    #[default]
    Normal,
    High,
    Urgent,
}

impl Priority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Priority::Silent => "silent",
            Priority::Normal => "normal",
            Priority::High => "high",
            Priority::Urgent => "urgent",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(untagged)]
enum OneOrMany {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn into_vec(self) -> Vec<String> {
        match self {
            OneOrMany::None => vec![],
            OneOrMany::One(s) => vec![s],
            OneOrMany::Many(v) => v,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct WhenSpec {
    #[serde(default)]
    from: OneOrMany,
    from_name: Option<String>,
    subject: Option<String>,
    body: Option<String>,
    #[serde(default)]
    labels_any: Vec<String>,
    #[serde(default)]
    labels_none: Vec<String>,
    list_unsubscribe: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Example {
    pub from: String,
    pub subject: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub list_unsubscribe: bool,
    #[serde(default)]
    pub body: String,
    /// Expected extracted values, checked when present.
    #[serde(default)]
    pub expect: BTreeMap<String, String>,
}

impl Example {
    pub fn to_message(&self) -> ParsedMessage {
        let (from_name, from_addr) = parse_address(&self.from);
        ParsedMessage {
            id: "example".into(),
            thread_id: "example".into(),
            label_ids: self.labels.clone(),
            from_name,
            from_addr,
            subject: self.subject.clone(),
            body_text: self.body.clone(),
            has_list_unsubscribe: self.list_unsubscribe,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleSpec {
    name: String,
    #[serde(default)]
    when: WhenSpec,
    action: Action,
    priority: Option<Priority>,
    summarize: Option<bool>,
    section: Option<String>,
    message: Option<String>,
    #[serde(default)]
    extract: BTreeMap<String, String>,
    #[serde(default)]
    examples: Vec<Example>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileSpec {
    #[serde(default)]
    default: Action,
    #[serde(default)]
    rules: Vec<RuleSpec>,
}

#[derive(Debug)]
struct When {
    from: Vec<Regex>,
    from_name: Option<Regex>,
    subject: Option<Regex>,
    body: Option<Regex>,
    labels_any: Vec<String>,
    labels_none: Vec<String>,
    list_unsubscribe: Option<bool>,
}

#[derive(Debug)]
pub struct Rule {
    pub name: String,
    when: When,
    pub action: Action,
    pub priority: Priority,
    pub summarize: bool,
    pub section: Option<String>,
    pub message: Option<String>,
    extract: Vec<(String, Regex)>,
    pub examples: Vec<Example>,
}

#[derive(Debug, Default)]
pub struct RuleSet {
    pub default: Action,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub rule: String,
    pub action: Action,
    pub priority: Priority,
    pub summarize: bool,
    pub section: Option<String>,
    /// Rendered `message` template (rule-only notifications), if the rule has one.
    pub message: Option<String>,
    pub extracted: BTreeMap<String, String>,
    /// Why model triage chose this action (`rule = "model"` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Body text examined by `body` matchers and extractors (keeps regex work bounded).
const BODY_SCAN_BYTES: usize = 4096;

fn ci_regex(pattern: &str, what: &str, rule: &str) -> anyhow::Result<Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .with_context(|| format!("rule {rule:?}: invalid {what} regex {pattern:?}"))
}

/// `*@shop.rakuten.co.jp` → anchored, case-insensitive regex. Only `*` is special.
pub fn glob_to_regex(glob: &str) -> Regex {
    let pattern = glob
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*");
    RegexBuilder::new(&format!("^{pattern}$"))
        .case_insensitive(true)
        .build()
        .expect("escaped glob")
}

fn head(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl When {
    fn matches(&self, m: &ParsedMessage) -> bool {
        let mut any = false;
        if !self.from.is_empty() {
            any = true;
            if !self.from.iter().any(|g| g.is_match(&m.from_addr)) {
                return false;
            }
        }
        for (re, text) in [
            (&self.from_name, m.from_name.as_str()),
            (&self.subject, m.subject.as_str()),
        ] {
            if let Some(re) = re {
                any = true;
                if !re.is_match(text) {
                    return false;
                }
            }
        }
        if let Some(re) = &self.body {
            any = true;
            if !re.is_match(head(&m.body_text, BODY_SCAN_BYTES)) {
                return false;
            }
        }
        if !self.labels_any.is_empty() {
            any = true;
            if !self.labels_any.iter().any(|l| m.label_ids.contains(l)) {
                return false;
            }
        }
        if !self.labels_none.is_empty() {
            any = true;
            if self.labels_none.iter().any(|l| m.label_ids.contains(l)) {
                return false;
            }
        }
        if let Some(lu) = self.list_unsubscribe {
            any = true;
            if m.has_list_unsubscribe != lu {
                return false;
            }
        }
        any
    }
}

impl Rule {
    fn verdict(&self, m: &ParsedMessage) -> Verdict {
        let text = format!("{}\n{}", m.subject, head(&m.body_text, BODY_SCAN_BYTES));
        let extracted: BTreeMap<String, String> = self
            .extract
            .iter()
            .filter_map(|(name, re)| {
                let caps = re.captures(&text)?;
                let v = caps
                    .get(1)
                    .or_else(|| caps.get(0))?
                    .as_str()
                    .trim()
                    .to_string();
                Some((name.clone(), v))
            })
            .collect();
        let message = self.message.as_ref().map(|t| render(t, &extracted, m));
        Verdict {
            rule: self.name.clone(),
            action: self.action,
            priority: self.priority,
            summarize: self.summarize,
            section: self.section.clone(),
            message,
            extracted,
            reason: None,
        }
    }
}

/// Replace `{name}` with extracted values; `{subject}` and `{from}` are always available.
fn render(template: &str, extracted: &BTreeMap<String, String>, m: &ParsedMessage) -> String {
    let mut out = template.to_string();
    for (k, v) in extracted {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out = out
        .replace("{subject}", &m.subject)
        .replace("{from}", &m.sender());
    static LEFT: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\{[a-z_]+\}").unwrap());
    LEFT.replace_all(&out, "?").into_owned()
}

impl RuleSet {
    pub fn parse(yaml: &str) -> anyhow::Result<Self> {
        let spec: FileSpec = serde_yaml::from_str(yaml).context("invalid rules YAML")?;
        let mut rules = Vec::with_capacity(spec.rules.len());
        let mut names = std::collections::HashSet::new();
        for r in spec.rules {
            anyhow::ensure!(
                names.insert(r.name.clone()),
                "duplicate rule name {:?}",
                r.name
            );
            let w = r.when;
            let when = When {
                from: w.from.into_vec().iter().map(|g| glob_to_regex(g)).collect(),
                from_name: w
                    .from_name
                    .as_deref()
                    .map(|p| ci_regex(p, "from_name", &r.name))
                    .transpose()?,
                subject: w
                    .subject
                    .as_deref()
                    .map(|p| ci_regex(p, "subject", &r.name))
                    .transpose()?,
                body: w
                    .body
                    .as_deref()
                    .map(|p| ci_regex(p, "body", &r.name))
                    .transpose()?,
                labels_any: w.labels_any,
                labels_none: w.labels_none,
                list_unsubscribe: w.list_unsubscribe,
            };
            anyhow::ensure!(
                !when.from.is_empty()
                    || when.from_name.is_some()
                    || when.subject.is_some()
                    || when.body.is_some()
                    || !when.labels_any.is_empty()
                    || !when.labels_none.is_empty()
                    || when.list_unsubscribe.is_some(),
                "rule {:?} has no conditions",
                r.name
            );
            let extract = r
                .extract
                .iter()
                .map(|(k, p)| Ok((k.clone(), ci_regex(p, &format!("extract.{k}"), &r.name)?)))
                .collect::<anyhow::Result<Vec<_>>>()?;
            rules.push(Rule {
                summarize: r
                    .summarize
                    .unwrap_or(matches!(r.action, Action::Notify | Action::Triage)),
                priority: r.priority.unwrap_or_default(),
                section: match r.action {
                    Action::Digest => Some(r.section.unwrap_or_else(|| "other".into())),
                    _ => r.section,
                },
                name: r.name,
                when,
                action: r.action,
                message: r.message,
                extract,
                examples: r.examples,
            });
        }
        Ok(Self {
            default: spec.default,
            rules,
        })
    }

    /// Digest sections in rule order (first appearance), ending with `other`. The digest renders
    /// sections in this order, then the model's section last.
    pub fn digest_sections(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for r in &self.rules {
            if let (Action::Digest, Some(s)) = (r.action, &r.section)
                && !out.contains(s)
            {
                out.push(s.clone());
            }
        }
        if !out.iter().any(|s| s == "other") {
            out.push("other".into());
        }
        out
    }

    pub fn evaluate(&self, m: &ParsedMessage) -> Verdict {
        for r in &self.rules {
            if r.when.matches(m) {
                return r.verdict(m);
            }
        }
        Verdict {
            rule: "default".into(),
            action: self.default,
            priority: Priority::Normal,
            summarize: matches!(self.default, Action::Notify | Action::Triage),
            section: matches!(self.default, Action::Digest).then(|| "other".into()),
            message: None,
            extracted: BTreeMap::new(),
            reason: None,
        }
    }

    /// Every example must be matched first by the rule it belongs to (and extract what it expects).
    pub fn check_examples(&self) -> Vec<String> {
        let mut errors = Vec::new();
        for r in &self.rules {
            for (i, ex) in r.examples.iter().enumerate() {
                let v = self.evaluate(&ex.to_message());
                if v.rule != r.name {
                    errors.push(format!(
                        "{}: example #{} matched {:?} instead",
                        r.name,
                        i + 1,
                        v.rule
                    ));
                    continue;
                }
                for (k, want) in &ex.expect {
                    let got = v.extracted.get(k).map(String::as_str).unwrap_or("<none>");
                    if got != want {
                        errors.push(format!(
                            "{}: example #{} extracted {k}={got:?}, expected {want:?}",
                            r.name,
                            i + 1
                        ));
                    }
                }
            }
        }
        errors
    }
}

/// Loads `rules.yaml` and reloads it when its modification time changes. An invalid file keeps the
/// previous rules and logs the error.
pub struct RuleFile {
    path: PathBuf,
    state: Mutex<(Option<SystemTime>, Arc<RuleSet>, bool)>,
}

impl RuleFile {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            state: Mutex::new((None, Arc::new(RuleSet::default()), false)),
        }
    }

    pub fn get(&self) -> Arc<RuleSet> {
        let mut st = self.state.lock().unwrap();
        let mtime = match std::fs::metadata(&self.path).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => {
                if !st.2 {
                    tracing::warn!(path = %self.path.display(), "rules file not found; every mail goes to the default action");
                    st.2 = true;
                }
                return st.1.clone();
            }
        };
        if st.0 != Some(mtime) {
            match std::fs::read_to_string(&self.path)
                .map_err(anyhow::Error::from)
                .and_then(|s| RuleSet::parse(&s))
            {
                Ok(rs) => {
                    tracing::info!(path = %self.path.display(), rules = rs.rules.len(), "loaded triage rules");
                    *st = (Some(mtime), Arc::new(rs), false);
                }
                Err(e) => {
                    tracing::error!(path = %self.path.display(), error = %format!("{e:#}"), "invalid rules file; keeping previous rules");
                    st.0 = Some(mtime);
                }
            }
        }
        st.1.clone()
    }
}
