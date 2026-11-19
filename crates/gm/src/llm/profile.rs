//! Sampling overrides by model in `GM_LLM_PROFILE` (default `llm.yaml`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, de::IgnoredAny};

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Sampling {
    temperature: Option<f32>,
    thinking_temperature: Option<f32>,
    pub(super) top_p: Option<f32>,
    #[serde(default, rename = "no_think_tag")]
    _legacy_no_think_tag: IgnoredAny,
    #[serde(default, rename = "tiers")]
    _legacy_tiers: IgnoredAny,
}

impl Sampling {
    pub(super) fn direct_temperature(self) -> f32 {
        self.temperature.unwrap_or(0.2)
    }

    pub(super) fn thinking_temperature(self) -> f32 {
        self.thinking_temperature
            .or(self.temperature)
            .unwrap_or(0.6)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    models: BTreeMap<String, Sampling>,
}

pub(crate) struct Profiles {
    path: PathBuf,
    models: Option<BTreeMap<String, Sampling>>,
}

impl Profiles {
    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path: path.to_path_buf(),
                models: None,
            }),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn parse(text: &str, path: &Path) -> anyhow::Result<Self> {
        let file: ProfileFile = serde_yaml::from_str(text)
            .with_context(|| format!("{} is not a valid LLM sampling profile", path.display()))?;
        for (model, sampling) in &file.models {
            for (name, value) in [
                ("temperature", sampling.temperature),
                ("thinking_temperature", sampling.thinking_temperature),
            ] {
                anyhow::ensure!(
                    value.is_none_or(|v| v.is_finite() && v >= 0.0),
                    "{}: {model}: {name} must be a finite, non-negative number",
                    path.display()
                );
            }
            anyhow::ensure!(
                sampling.top_p.is_none_or(|v| v > 0.0 && v <= 1.0),
                "{}: {model}: top_p must be greater than 0 and at most 1",
                path.display()
            );
        }
        Ok(Self {
            path: path.to_path_buf(),
            models: Some(file.models),
        })
    }

    pub(super) fn sampling(&self, model: &str) -> Sampling {
        self.models
            .as_ref()
            .and_then(|models| models.get(model))
            .copied()
            .unwrap_or_default()
    }

    pub(crate) fn describe(&self, model: &str, client: &str) -> (String, Option<String>) {
        let source = match &self.models {
            Some(models) if models.contains_key(model) => None,
            Some(_) => Some(format!("no section for {model} in {}", self.path.display())),
            None => Some(format!("{} not found", self.path.display())),
        };
        let sampling = self.sampling(model);
        let top_p = sampling
            .top_p
            .map_or_else(|| "server default".into(), |p| p.to_string());
        let path = if source.is_none() {
            self.path.display().to_string()
        } else {
            "built-in sampling defaults".into()
        };
        let detail = format!(
            "{model} ({client} client, {path}): temperature {}, thinking temperature {}, top_p {top_p}",
            sampling.direct_temperature(),
            sampling.thinking_temperature()
        );
        (detail, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_profiles_load_with_sampling_defaults() {
        let profiles = Profiles::parse(
            "models: {q: {no_think_tag: /no_think, tiers: {hard: {thinking: true}}}}",
            Path::new("llm.yaml"),
        )
        .unwrap();
        let sampling = profiles.sampling("q");
        assert_eq!(sampling.direct_temperature(), 0.2);
        assert_eq!(sampling.thinking_temperature(), 0.6);
        assert!(sampling.top_p.is_none());
    }

    #[test]
    fn invalid_sampling_is_rejected() {
        for field in [
            "temprature: 0.7",
            "temperature: -0.1",
            "thinking_temperature: .inf",
            "top_p: 0",
            "top_p: 1.1",
        ] {
            let yaml = format!("models: {{q: {{{field}}}}}");
            assert!(Profiles::parse(&yaml, Path::new("llm.yaml")).is_err());
        }
    }

    #[test]
    fn the_repo_profile_is_valid_and_covers_the_default_model() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../llm.yaml");
        let profiles = Profiles::load(&path).unwrap();
        assert!(
            profiles
                .models
                .unwrap()
                .contains_key(crate::config::DEFAULT_LLM_MODEL)
        );
    }
}
