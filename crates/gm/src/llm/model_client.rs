//! Model-specific inference plans. The shared client executes these over its HTTP transport.

mod bonsai;
mod qwen3;

use std::collections::HashMap;
use std::path::Path;

use linkme::distributed_slice;

pub(crate) use qwen3::QWEN3_MODEL;

/// How much reasoning a task needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tier {
    Easy,
    Medium,
    Hard,
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

    fn tier(&self) -> Tier {
        match self {
            Task::Router | Task::Triage | Task::DigestLine => Tier::Easy,
            Task::Summary | Task::DigestJobs | Task::Translate => Tier::Medium,
            Task::Analyze | Task::Reply => Tier::Hard,
        }
    }
}

trait ModelClient: Sync {
    fn easy(&self, task: Task) -> Inference;
    fn medium(&self, task: Task) -> Inference;
    fn hard(&self, task: Task) -> Inference;
}

#[derive(Clone, Copy)]
pub(super) enum AnswerTokens {
    Fixed(u32),
    ConfiguredSummary,
}

impl AnswerTokens {
    pub(super) fn resolve(self, summary_cap: u32) -> u32 {
        match self {
            Self::Fixed(cap) => cap,
            Self::ConfiguredSummary => summary_cap,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Inference {
    pub(super) answer: AnswerTokens,
    pub(super) mode: Mode,
}

#[derive(Clone, Copy)]
pub(super) enum Mode {
    Direct {
        system_suffix: &'static str,
    },
    Thinking {
        reasoning_tokens: u32,
        decode_answer: fn(String) -> String,
        fallback_tag: Option<&'static str>,
        language_check: bool,
    },
}

impl Inference {
    pub(super) fn system_suffix(self) -> &'static str {
        match self.mode {
            Mode::Direct { system_suffix } => system_suffix,
            Mode::Thinking { .. } => "",
        }
    }
}

struct Registration {
    name: &'static str,
    models: &'static [&'static str],
    client: &'static dyn ModelClient,
    fallback: bool,
}

#[distributed_slice]
static CLIENTS: [Registration];

pub(crate) struct ClientRegistry {
    models: HashMap<&'static str, &'static Registration>,
    fallback: &'static Registration,
}

impl ClientRegistry {
    pub(crate) fn load() -> anyhow::Result<Self> {
        let mut models = HashMap::new();
        let mut fallback = None;
        for registration in CLIENTS {
            if registration.fallback && fallback.replace(registration).is_some() {
                anyhow::bail!("multiple fallback model clients registered");
            }
            for &model in registration.models {
                if let Some(previous) = models.insert(model, registration) {
                    anyhow::bail!(
                        "model {model} registered by both {} and {}",
                        previous.name,
                        registration.name
                    );
                }
            }
        }
        let fallback =
            fallback.ok_or_else(|| anyhow::anyhow!("no fallback model client registered"))?;
        Ok(Self { models, fallback })
    }

    pub(super) fn inference_for(&self, model: &str, task: Task) -> Inference {
        let registration = self.registration(model).unwrap_or(self.fallback);
        match task.tier() {
            Tier::Easy => registration.client.easy(task),
            Tier::Medium => registration.client.medium(task),
            Tier::Hard => registration.client.hard(task),
        }
    }

    /// Registered local checkpoints use their directory name as the client and sampling key.
    pub(crate) fn profile_key<'a>(&self, model: &'a str) -> &'a str {
        if self.models.contains_key(model) {
            return model;
        }
        let path = Path::new(model);
        if path.is_absolute()
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
            && self.models.contains_key(name)
        {
            return name;
        }
        model
    }

    fn registration(&self, model: &str) -> Option<&'static Registration> {
        self.models.get(self.profile_key(model)).copied()
    }

    pub(crate) fn client_name(&self, model: &str) -> String {
        match self.registration(model) {
            Some(registration) => registration.name.into(),
            None => format!("{} fallback", self.fallback.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AliasClient;

    impl ModelClient for AliasClient {
        fn easy(&self, task: Task) -> Inference {
            let mode = match task {
                Task::Triage | Task::DigestLine => Mode::Thinking {
                    reasoning_tokens: if task == Task::Triage { 100 } else { 200 },
                    decode_answer: std::convert::identity,
                    fallback_tag: None,
                    language_check: false,
                },
                _ => Mode::Direct {
                    system_suffix: "alias-client",
                },
            };
            Inference {
                answer: AnswerTokens::Fixed(80),
                mode,
            }
        }

        fn medium(&self, task: Task) -> Inference {
            self.easy(task)
        }

        fn hard(&self, task: Task) -> Inference {
            self.easy(task)
        }
    }

    #[distributed_slice(CLIENTS)]
    static ALIAS_CLIENT: Registration = Registration {
        name: "Alias client",
        models: &["test/one", "test/two"],
        client: &AliasClient,
        fallback: false,
    };

    #[test]
    fn registrations_supply_all_model_ids_without_a_selector_entry() {
        let registry = ClientRegistry::load().unwrap();
        for id in ["test/one", "test/two"] {
            assert_eq!(registry.client_name(id), "Alias client");
            assert_eq!(
                registry.inference_for(id, Task::Router).system_suffix(),
                "alias-client"
            );
        }
        assert_eq!(registry.client_name("test/unknown"), "Qwen3 fallback");
    }

    #[tokio::test]
    async fn a_client_can_send_different_budgets_for_tasks_in_the_same_tier() {
        use crate::config::Config;
        use crate::llm::{Lane, LlmClient, parse_json_object};
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "{\"ok\":true}"}}]
            })))
            .mount(&server)
            .await;
        let mut cfg = Config::for_tests();
        cfg.llm_model = "test/one".into();
        cfg.llm_base_url = format!("{}/v1", server.uri());
        let llm = LlmClient::new(&cfg, None).unwrap();

        for task in [Task::Triage, Task::DigestLine] {
            let reply = llm
                .run(
                    task,
                    Lane::Notify,
                    "Return a JSON object.",
                    "English",
                    "<email>Meeting moved to Friday.</email>",
                    parse_json_object,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(reply.parsed.unwrap()["ok"], true);
        }

        let requests = server.received_requests().await.unwrap();
        let caps: Vec<_> = requests
            .iter()
            .map(|request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                body["max_tokens"].as_u64().unwrap()
            })
            .collect();
        assert_eq!(caps, [180, 280]);
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
