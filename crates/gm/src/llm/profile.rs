//! The inference profile of each model: per difficulty tier, whether tasks think, how many extra
//! tokens the reasoning gets, and whether answers in the owner's language are checked. Loaded from
//! `GM_LLM_PROFILE` (`llm.yaml`), one section per model id; every call uses the section of the model
//! it is sent to. A missing file or section falls back to [`Profile::defaults`], an invalid file is
//! an error. Which task belongs to which tier is fixed here ([`Task::tier`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// How much reasoning a task needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Easy,
    Medium,
    Hard,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::Easy => "easy",
            Tier::Medium => "medium",
            Tier::Hard => "hard",
        }
    }
}

/// Every model task. `name` is the `llm_calls.task` value; `tier` is the only task → tier map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    Router,
    Triage,
    DigestLine,
    DigestJobs,
    Summary,
    Translate,
    Analyze,
    Reply,
}

impl Task {
    pub const ALL: [Task; 8] = [
        Task::Router,
        Task::Triage,
        Task::DigestLine,
        Task::DigestJobs,
        Task::Summary,
        Task::Translate,
        Task::Analyze,
        Task::Reply,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Task::Router => "router",
            Task::Triage => "triage",
            Task::DigestLine => "digest-line",
            Task::DigestJobs => "digest-jobs",
            Task::Summary => "summary",
            Task::Translate => "translate",
            Task::Analyze => "analyze",
            Task::Reply => "reply",
        }
    }

    /// Whether the answer is written in the owner's language (or the translation language): its
    /// prompt restates that language after the mail.
    pub fn in_owner_language(&self) -> bool {
        matches!(
            self,
            Task::Summary | Task::DigestLine | Task::Translate | Task::Analyze
        )
    }

    pub fn tier(&self) -> Tier {
        match self {
            Task::Router | Task::Triage | Task::DigestLine => Tier::Easy,
            Task::Summary | Task::DigestJobs | Task::Translate => Tier::Medium,
            Task::Analyze | Task::Reply => Tier::Hard,
        }
    }
}

/// The parameters of one tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierParams {
    /// Leave the no-think tag out, so the model reasons before answering.
    pub thinking: bool,
    /// Extra output tokens for the reasoning, added to the task's answer cap when `thinking`.
    #[serde(default)]
    pub thinking_budget: u32,
    /// Check that an answer in the owner's language is in that language, and ask once more if not.
    #[serde(default)]
    pub language_check: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tiers {
    easy: TierParams,
    medium: TierParams,
    hard: TierParams,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelSection {
    /// Appended to the system prompt to switch thinking off; empty for a model without a switch.
    no_think_tag: String,
    tiers: Tiers,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    models: BTreeMap<String, ModelSection>,
}

/// The whole profile file: every model's section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profiles {
    path: PathBuf,
    /// `false`: the file does not exist, so every model gets the defaults.
    found: bool,
    models: BTreeMap<String, ModelSection>,
}

impl Profiles {
    /// The file at `path`; a missing file is empty, one that doesn't parse or validate is an error.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path: path.to_path_buf(),
                found: false,
                models: BTreeMap::new(),
            }),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(text: &str, path: &Path) -> anyhow::Result<Self> {
        let file: ProfileFile = serde_yaml::from_str(text)
            .with_context(|| format!("{} is not a valid LLM profile", path.display()))?;
        for (name, section) in &file.models {
            for (tier, p) in [
                ("easy", &section.tiers.easy),
                ("medium", &section.tiers.medium),
                ("hard", &section.tiers.hard),
            ] {
                if p.thinking && p.thinking_budget == 0 {
                    anyhow::bail!(
                        "{}: {name}: tier {tier} thinks but has no thinking_budget",
                        path.display()
                    );
                }
                if !p.thinking && p.thinking_budget > 0 {
                    anyhow::bail!(
                        "{}: {name}: tier {tier} has a thinking_budget but thinking is off",
                        path.display()
                    );
                }
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            found: true,
            models: file.models,
        })
    }

    /// The profile for `model`: its section, else the defaults with the reason.
    pub fn for_model(&self, model: &str) -> Profile {
        match self.models.get(model) {
            Some(s) => Profile {
                model: model.to_string(),
                source: Source::File(self.path.clone()),
                no_think_tag: s.no_think_tag.trim().to_string(),
                tiers: s.tiers.clone(),
            },
            None if !self.found => {
                Profile::defaults(model, format!("{} not found", self.path.display()))
            }
            None => Profile::defaults(
                model,
                format!("no section for {model} in {}", self.path.display()),
            ),
        }
    }
}

/// Where the active profile came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    /// Built-in defaults, and why (no file, or no section for the model).
    Defaults(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub model: String,
    pub source: Source,
    pub no_think_tag: String,
    tiers: Tiers,
}

impl Profile {
    /// Qwen3's switch; the hard tier thinks with 1,024 tokens of reasoning; no tier checks language.
    pub fn defaults(model: &str, why: impl Into<String>) -> Self {
        let plain = TierParams {
            thinking: false,
            thinking_budget: 0,
            language_check: false,
        };
        Self {
            model: model.to_string(),
            source: Source::Defaults(why.into()),
            no_think_tag: "/no_think".into(),
            tiers: Tiers {
                easy: plain,
                medium: plain,
                hard: TierParams {
                    thinking: true,
                    thinking_budget: 1024,
                    language_check: false,
                },
            },
        }
    }

    pub fn tier(&self, tier: Tier) -> TierParams {
        match tier {
            Tier::Easy => self.tiers.easy,
            Tier::Medium => self.tiers.medium,
            Tier::Hard => self.tiers.hard,
        }
    }

    pub fn params(&self, task: Task) -> TierParams {
        self.tier(task.tier())
    }

    /// One line for `gm status` and the startup log: which tiers think and which check language.
    pub fn summary(&self) -> String {
        let tiers = [Tier::Easy, Tier::Medium, Tier::Hard];
        let thinking: Vec<String> = tiers
            .iter()
            .filter(|t| self.tier(**t).thinking)
            .map(|t| format!("{} ({})", t.as_str(), self.tier(*t).thinking_budget))
            .collect();
        let checked: Vec<&str> = tiers
            .iter()
            .filter(|t| self.tier(**t).language_check)
            .map(Tier::as_str)
            .collect();
        let list = |v: Vec<String>| {
            if v.is_empty() {
                "none".to_string()
            } else {
                v.join(", ")
            }
        };
        let source = match &self.source {
            Source::File(p) => p.display().to_string(),
            Source::Defaults(_) => "built-in defaults".to_string(),
        };
        format!(
            "{} ({source}): thinking {}; language check {}",
            self.model,
            list(thinking),
            list(checked.into_iter().map(String::from).collect())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "
models:
  qwen:
    no_think_tag: /no_think
    tiers:
      easy:   {thinking: false}
      medium: {thinking: false, language_check: true}
      hard:   {thinking: true, thinking_budget: 2048, language_check: true}
";

    #[test]
    fn the_section_for_the_model_is_used() {
        let p = Profiles::parse(FILE, Path::new("llm.yaml"))
            .unwrap()
            .for_model("qwen");
        assert_eq!(p.source, Source::File("llm.yaml".into()));
        assert_eq!(p.params(Task::Analyze).thinking_budget, 2048);
        assert!(p.params(Task::Summary).language_check);
        assert!(!p.params(Task::Router).thinking);
        assert_eq!(
            p.summary(),
            "qwen (llm.yaml): thinking hard (2048); language check medium, hard"
        );
    }

    #[test]
    fn another_model_gets_the_defaults() {
        let p = Profiles::parse(FILE, Path::new("llm.yaml"))
            .unwrap()
            .for_model("other");
        assert_eq!(
            p,
            Profile::defaults("other", "no section for other in llm.yaml")
        );
        let missing = Profiles::load(Path::new("does-not-exist.yaml"))
            .unwrap()
            .for_model("m");
        assert_eq!(
            missing,
            Profile::defaults("m", "does-not-exist.yaml not found")
        );
    }

    #[test]
    fn invalid_profiles_are_rejected() {
        for bad in [
            // unknown key
            "models: {q: {no_think_tag: x, tiers: {easy: {thinking: false, effort: 1}, medium: {thinking: false}, hard: {thinking: false}}}}",
            // missing tier
            "models: {q: {no_think_tag: x, tiers: {easy: {thinking: false}, medium: {thinking: false}}}}",
            // thinking without a budget
            "models: {q: {no_think_tag: x, tiers: {easy: {thinking: true}, medium: {thinking: false}, hard: {thinking: false}}}}",
            // a budget without thinking
            "models: {q: {no_think_tag: x, tiers: {easy: {thinking: false, thinking_budget: 5}, medium: {thinking: false}, hard: {thinking: false}}}}",
        ] {
            assert!(
                Profiles::parse(bad, Path::new("llm.yaml")).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_repo_profile_is_valid_and_covers_the_default_model() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../llm.yaml");
        let p = Profiles::load(&path)
            .unwrap()
            .for_model(crate::config::DEFAULT_LLM_MODEL);
        assert!(matches!(p.source, Source::File(_)), "{:?}", p.source);
    }

    #[test]
    fn every_task_has_a_tier_and_a_distinct_name() {
        let mut names: Vec<&str> = Task::ALL.iter().map(Task::name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), Task::ALL.len());
        assert_eq!(Task::Analyze.tier(), Tier::Hard);
        assert_eq!(Task::Translate.tier(), Tier::Medium);
        assert_eq!(Task::Triage.tier(), Tier::Easy);
    }
}
