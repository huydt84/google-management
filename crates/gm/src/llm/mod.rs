//! All model calls go through [`LlmClient`] (invariant 7, ADR-0010): an OpenAI-compatible
//! `POST /chat/completions` to mlx-server-runtime, admitted through priority lanes, recorded in
//! `llm_calls`. No `tools` / `response_format`: prompts ask for one JSON object and the output is
//! parsed tolerantly, with one retry. Each task's tier in the model's [`profile`] decides whether
//! it thinks and whether its answer's language is checked ([`LlmClient::run`]).

pub mod clean;
pub mod language;
pub mod profile;

pub use profile::{Profile, Profiles, Task, Tier};

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::oneshot;

use crate::config::Config;
use crate::gmail::message::ParsedMessage;

pub const SYSTEM_PREFIX: &str = include_str!("../../prompts/system.md");
pub const SUMMARY_TASK: &str = include_str!("../../prompts/summary.md");

/// The model server can't take the request right now (not running, timed out, or overloaded:
/// connection error, timeout, HTTP 429/502/503/504). Callers queue the work instead of failing it
/// (ADR-0010).
#[derive(Debug, thiserror::Error)]
#[error("LLM unavailable: {0}")]
pub struct LlmUnavailable(pub String);

impl LlmUnavailable {
    pub fn is(e: &anyhow::Error) -> bool {
        e.downcast_ref::<LlmUnavailable>().is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// User-initiated requests (replies, translations): never wait behind background work.
    Interactive = 0,
    /// Triage and summaries of incoming mail.
    Notify = 1,
    /// Digest items and other background batches.
    Batch = 2,
}

impl Lane {
    pub fn as_str(&self) -> &'static str {
        match self {
            Lane::Interactive => "interactive",
            Lane::Notify => "notify",
            Lane::Batch => "batch",
        }
    }
}

struct AdmissionState {
    inflight: usize,
    waiting: [VecDeque<oneshot::Sender<()>>; 3],
}

/// At most `max` requests in flight; when a slot frees up, the highest-priority waiting lane goes
/// first (FIFO within a lane).
pub struct Admission {
    max: usize,
    state: Mutex<AdmissionState>,
}

pub struct Permit {
    admission: Arc<Admission>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.admission.release();
    }
}

impl Admission {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            max: max.max(1),
            state: Mutex::new(AdmissionState {
                inflight: 0,
                waiting: Default::default(),
            }),
        })
    }

    pub async fn acquire(self: &Arc<Self>, lane: Lane) -> Permit {
        let rx = {
            let mut st = self.state.lock().unwrap();
            if st.inflight < self.max && st.waiting.iter().all(VecDeque::is_empty) {
                st.inflight += 1;
                return Permit {
                    admission: self.clone(),
                };
            }
            let (tx, rx) = oneshot::channel();
            st.waiting[lane as usize].push_back(tx);
            rx
        };
        // The releaser counted us in before sending.
        let _ = rx.await;
        Permit {
            admission: self.clone(),
        }
    }

    fn release(&self) {
        let mut st = self.state.lock().unwrap();
        st.inflight -= 1;
        for q in 0..3 {
            while let Some(tx) = st.waiting[q].pop_front() {
                st.inflight += 1;
                if tx.send(()).is_ok() {
                    return;
                }
                st.inflight -= 1; // waiter gave up
            }
        }
    }

    pub fn inflight(&self) -> usize {
        self.state.lock().unwrap().inflight
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmailSummary {
    pub summary: String,
    #[serde(default = "medium")]
    pub importance: String,
    #[serde(default = "other")]
    pub category: String,
    #[serde(default)]
    pub suggested_actions: Vec<SuggestedAction>,
    #[serde(default)]
    pub language: String,
    #[serde(default = "yes")]
    pub parse_ok: bool,
}

/// One suggested next step. `url` is set only when the model picked one of the trusted links
/// it was shown ([`ParsedMessage::trusted_links`]); the model never writes a URL itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(from = "RawAction")]
pub struct SuggestedAction {
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

/// Stored summaries written before links existed hold plain strings.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawAction {
    Text(String),
    Full {
        text: String,
        #[serde(default)]
        url: String,
    },
}

impl From<RawAction> for SuggestedAction {
    fn from(raw: RawAction) -> Self {
        match raw {
            RawAction::Text(text) => Self {
                text,
                url: String::new(),
            },
            RawAction::Full { text, url } => Self { text, url },
        }
    }
}

impl SuggestedAction {
    /// The suggestions of a stored `events.summary` (strings or objects); unusable items are skipped.
    pub fn list(summary: &Value) -> Vec<Self> {
        summary["suggested_actions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| serde_json::from_value::<Self>(v.clone()).ok())
                    .filter(|a| !a.text.trim().is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The host shown next to a linked suggestion, so the owner sees where it goes.
    pub fn host(&self) -> Option<String> {
        url::Url::parse(&self.url)
            .ok()?
            .host_str()
            .map(str::to_string)
    }
}

fn medium() -> String {
    "medium".into()
}
fn other() -> String {
    "other".into()
}
fn yes() -> bool {
    true
}

/// Remove the model's reasoning: `<think>…</think>` blocks, and an unclosed `<think>` of a reply
/// cut off by the token cap (nothing after it is an answer). Trimmed.
pub fn strip_think(text: &str) -> String {
    static THINK: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?is)<think>.*?</think>").unwrap());
    let cleaned = THINK.replace_all(text, "");
    let cleaned = match cleaned.to_ascii_lowercase().find("<think>") {
        Some(i) => &cleaned[..i],
        None => &cleaned[..],
    };
    cleaned.trim().to_string()
}

/// Strip reasoning and fences and parse the outermost `{...}` as a JSON object. `None` if there is
/// none. Every task parses model output through this (the runtime has no JSON mode, so parsing is tolerant).
pub fn parse_json_object(text: &str) -> Option<serde_json::Map<String, Value>> {
    let cleaned = strip_think(text);
    let (start, end) = (cleaned.find('{')?, cleaned.rfind('}')?);
    if end <= start {
        return None;
    }
    match serde_json::from_str(&cleaned[start..=end]).ok()? {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

/// Validate a summary. `None` if unusable. `links` are the trusted links the prompt listed: an
/// action's `"link": n` becomes `links[n - 1]`; any other value leaves the action unlinked.
pub fn parse_summary(text: &str, links: &[String]) -> Option<EmailSummary> {
    let obj = parse_json_object(text)?;
    let summary = obj.get("summary")?.as_str()?.trim().to_string();
    if summary.is_empty() {
        return None;
    }
    let importance = obj
        .get("importance")
        .and_then(Value::as_str)
        .unwrap_or("medium")
        .to_lowercase();
    Some(EmailSummary {
        summary,
        importance: if ["low", "medium", "high"].contains(&importance.as_str()) {
            importance
        } else {
            medium()
        },
        category: obj
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("other")
            .to_string(),
        suggested_actions: obj
            .get("suggested_actions")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| parse_action(x, links))
                    .take(3)
                    .collect()
            })
            .unwrap_or_default(),
        language: obj
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        parse_ok: true,
    })
}

fn parse_action(item: &Value, links: &[String]) -> Option<SuggestedAction> {
    let (text, link) = match item {
        Value::String(t) => (t.as_str(), None),
        Value::Object(o) => (
            o.get("text")?.as_str()?,
            o.get("link").and_then(Value::as_u64),
        ),
        _ => return None,
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let url = link
        .and_then(|n| usize::try_from(n).ok()?.checked_sub(1))
        .and_then(|i| links.get(i))
        .cloned()
        .unwrap_or_default();
    Some(SuggestedAction {
        text: text.to_string(),
        url,
    })
}

/// A task's messages. With `think` the no-think `tag` is simply not appended.
fn task_messages(
    tag: &str,
    prompt: &str,
    language: &str,
    email_block: &str,
    think: bool,
) -> Vec<Value> {
    let mut system = format!("{}\n{}", SYSTEM_PREFIX.trim_end(), prompt.trim_end());
    if !think && !tag.is_empty() {
        system.push_str(&format!("\n{tag}"));
    }
    vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": format!("Output language: {language}\n\n{email_block}")}),
    ]
}

pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    pub model: String,
    profiles: Profiles,
    /// Models already reported as running on the built-in defaults.
    warned: Mutex<std::collections::BTreeSet<String>>,
    max_input_tokens: usize,
    summary_max_tokens: u32,
    admission: Arc<Admission>,
    pool: Option<PgPool>,
    recorder: Option<Arc<Mutex<Vec<CallRecord>>>>,
    control: Option<Arc<crate::model::ModelControl>>,
}

/// One model request as seen by [`LlmClient::record_calls`] (used by `gm eval`, which has no DB).
#[derive(Debug, Clone)]
pub struct CallRecord {
    pub task: String,
    pub ok: bool,
    pub latency_ms: i32,
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
}

/// Result of a JSON task: the parsed value (after at most one retry) and the last raw output.
pub struct JsonReply<T> {
    pub parsed: Option<T>,
    pub raw: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}
#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
}
#[derive(Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
}
#[derive(Deserialize, Default)]
struct Usage {
    prompt_tokens: Option<i32>,
    completion_tokens: Option<i32>,
}

impl LlmClient {
    /// Loads the inference profiles (`GM_LLM_PROFILE`); an invalid file is an error.
    pub fn new(cfg: &Config, pool: Option<PgPool>) -> anyhow::Result<Self> {
        let profiles = Profiles::load(&cfg.llm_profile_path)?;
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(cfg.llm_timeout_s))
                .build()
                .expect("reqwest client"),
            base_url: cfg.llm_base_url.clone(),
            api_key: cfg.llm_api_key.clone(),
            model: cfg.llm_model.clone(),
            profiles,
            warned: Mutex::new(Default::default()),
            max_input_tokens: cfg.llm_max_input_tokens,
            summary_max_tokens: cfg.llm_summary_max_tokens,
            admission: Admission::new(cfg.llm_max_inflight),
            pool,
            recorder: None,
            control: None,
        })
    }

    /// The profile of the model requests go to now. Logged once per model: info from the file,
    /// a warning on the built-in defaults.
    pub fn profile(&self) -> Profile {
        let profile = self.profiles.for_model(&self.request_model());
        if self.warned.lock().unwrap().insert(profile.model.clone()) {
            match &profile.source {
                profile::Source::File(_) => {
                    tracing::info!(profile = %profile.summary(), "LLM profile")
                }
                profile::Source::Defaults(why) => {
                    tracing::warn!(reason = %why, profile = %profile.summary(), "LLM profile: built-in defaults")
                }
            }
        }
        profile
    }

    /// The model id requests carry: the served one when gm controls the model, else `GM_LLM_MODEL`.
    fn request_model(&self) -> String {
        self.control
            .as_ref()
            .map_or_else(|| self.model.clone(), |c| c.request_model())
    }

    /// Let the model control start the model for owner requests and count calls in flight.
    pub fn with_control(mut self, control: Arc<crate::model::ModelControl>) -> Self {
        self.control = Some(control);
        self
    }

    /// Keep every call in memory as well (tokens, latency, ok) and return the shared list.
    pub fn record_calls(&mut self) -> Arc<Mutex<Vec<CallRecord>>> {
        let r = Arc::new(Mutex::new(Vec::new()));
        self.recorder = Some(r.clone());
        r
    }

    pub async fn chat(
        &self,
        task: &str,
        lane: Lane,
        messages: &[Value],
        max_tokens: u32,
        temperature: f32,
    ) -> anyhow::Result<String> {
        // An owner request starts the model if it is down; every call counts as in flight.
        let _running = match &self.control {
            Some(c) => Some(c.begin(lane).await?),
            None => None,
        };
        let model = self.request_model();
        let _permit = self.admission.acquire(lane).await;
        let started = Instant::now();
        let result = async {
            let resp = self
                .http
                .post(format!("{}/chat/completions", self.base_url))
                .bearer_auth(&self.api_key)
                .json(&json!({
                    "model": model, "messages": messages, "max_tokens": max_tokens,
                    "temperature": temperature, "stream": false
                }))
                .send()
                .await
                .map_err(|e| LlmUnavailable(format!("request failed: {}", e.without_url())))?;
            let status = resp.status();
            if !status.is_success() {
                let body: String = resp
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect();
                if matches!(status.as_u16(), 429 | 502 | 503 | 504) {
                    return Err(LlmUnavailable(format!("{status}: {body}")).into());
                }
                anyhow::bail!("LLM returned {status}: {body}");
            }
            let r: ChatResponse = resp.json().await?;
            let usage = r.usage.unwrap_or_default();
            let content = r
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.message.content)
                .unwrap_or_default();
            Ok((content, usage))
        }
        .await;
        let latency_ms = started.elapsed().as_millis() as i32;
        if let Some(rec) = &self.recorder {
            let (pt, ct) = match &result {
                Ok((_, u)) => (u.prompt_tokens, u.completion_tokens),
                Err(_) => (None, None),
            };
            rec.lock().unwrap().push(CallRecord {
                task: task.to_string(),
                ok: result.is_ok(),
                latency_ms,
                prompt_tokens: pt,
                completion_tokens: ct,
            });
        }
        if let Some(pool) = &self.pool {
            let (pt, ct, ok, err) = match &result {
                Ok((_, u)) => (u.prompt_tokens, u.completion_tokens, true, None),
                Err(e) => (None, None, false, Some(e.to_string())),
            };
            let _ = sqlx::query(
                "INSERT INTO llm_calls (task, lane, model, prompt_tokens, completion_tokens, latency_ms, ok, error)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(task)
            .bind(lane.as_str())
            .bind(&model)
            .bind(pt)
            .bind(ct)
            .bind(latency_ms)
            .bind(ok)
            .bind(err)
            .execute(pool)
            .await;
        }
        result.map(|(c, _)| c)
    }

    /// The mail as the model sees it: headers and the cleaned body capped at `max_tokens`, with
    /// delimiters neutralized and wrapped in `<email>` (Google content is untrusted).
    pub fn email_block(&self, m: &ParsedMessage, max_tokens: usize) -> String {
        let body = clean::clean_body(if m.body_text.is_empty() {
            &m.snippet
        } else {
            &m.body_text
        });
        let (body, truncated) =
            clean::truncate_to_tokens(&body, max_tokens.min(self.max_input_tokens));
        let attachments = if m.attachments.is_empty() {
            "none".to_string()
        } else {
            m.attachments.join(", ")
        };
        let email = format!(
            "From: {}\nTo: {}\nDate: {}\nSubject: {}\nAttachments: {}\n\n{}{}",
            m.sender(),
            m.to.join(", "),
            m.date,
            m.subject,
            attachments,
            body,
            if truncated { "\n[...truncated...]" } else { "" }
        );
        format!("<email>\n{}\n</email>", clean::sanitize_delimiters(&email))
    }

    /// The messages `task`'s first call sends: thinking as its tier says. Shared stable prefix first
    /// (system prompt + task prompt + thinking switch), per-user settings and the mail last, so the
    /// prefix KV cache is reused across mails and tasks.
    pub fn messages_for(
        &self,
        task: Task,
        prompt: &str,
        language: &str,
        email_block: &str,
    ) -> Vec<Value> {
        let profile = self.profile();
        let think = profile.params(task).thinking;
        task_messages(&profile.no_think_tag, prompt, language, email_block, think)
    }

    /// `links` are the mail's trusted links ([`ParsedMessage::trusted_links`]), listed after the
    /// mail so a suggested action can point at one by number.
    pub fn summary_messages(
        &self,
        m: &ParsedMessage,
        language: &str,
        links: &[String],
    ) -> Vec<Value> {
        let block = self.summary_block(m, links);
        self.messages_for(Task::Summary, SUMMARY_TASK, language, &block)
    }

    /// The mail, then its trusted links numbered in a `<links>` block.
    fn summary_block(&self, m: &ParsedMessage, links: &[String]) -> String {
        let mut block = self.email_block(m, self.max_input_tokens);
        if !links.is_empty() {
            block.push_str("\n\n<links>\n");
            for (i, l) in links.iter().enumerate() {
                block.push_str(&format!("[{}] {}\n", i + 1, clean::sanitize_delimiters(l)));
            }
            block.push_str("</links>");
        }
        block
    }

    /// Run one model task as its tier says. Thinking: one call with the reasoning budget added to
    /// `answer_cap` (temperature 0.6); an unusable answer, or one that fails the language check, is
    /// replaced by the answer without thinking. Without thinking: one JSON call with a format retry; with the
    /// language check on, an answer in the wrong language is asked once more at temperature 0. The
    /// last answer is used whatever its language. `checked` gives the text in `language` to check;
    /// `None` for tasks whose output is not in the owner's language (router, triage, reply).
    #[allow(clippy::too_many_arguments)]
    pub async fn run<T>(
        &self,
        task: Task,
        lane: Lane,
        prompt: &str,
        language: &str,
        block: &str,
        answer_cap: u32,
        parse: impl Fn(&str) -> Option<T>,
        checked: Option<&(dyn Fn(&T) -> String + Sync)>,
    ) -> anyhow::Result<JsonReply<T>> {
        let profile = self.profile();
        let params = profile.params(task);
        let name = task.name();
        let wrong_language = |v: &T| {
            params.language_check
                && checked.is_some_and(|f| language::in_language(language, &f(v)) == Some(false))
        };
        let plain = task_messages(&profile.no_think_tag, prompt, language, block, false);
        if params.thinking {
            let first = self
                .chat(
                    name,
                    lane,
                    &task_messages(&profile.no_think_tag, prompt, language, block, true),
                    answer_cap + params.thinking_budget,
                    0.6,
                )
                .await?;
            match parse(&first) {
                Some(v) if !wrong_language(&v) => {
                    return Ok(JsonReply {
                        parsed: Some(v),
                        raw: first,
                    });
                }
                Some(_) => tracing::warn!(
                    task = name,
                    "answer with thinking in the wrong language; asking without"
                ),
                None => {
                    tracing::warn!(task = name, "answer with thinking unusable; asking without")
                }
            }
            return self
                .chat_json(&format!("{name}-nothink"), lane, plain, answer_cap, parse)
                .await;
        }
        let reply = self
            .chat_json(name, lane, plain.clone(), answer_cap, &parse)
            .await?;
        if !reply.parsed.as_ref().is_some_and(&wrong_language) {
            return Ok(reply);
        }
        tracing::warn!(task = name, "answer in the wrong language; asking again");
        let again = self
            .chat(&format!("{name}-language"), lane, &plain, answer_cap, 0.0)
            .await?;
        Ok(match parse(&again) {
            Some(v) => JsonReply {
                parsed: Some(v),
                raw: again,
            },
            None => reply,
        })
    }

    /// Ask for one JSON object; if `parse` rejects the reply, ask once more with a format reminder
    /// at temperature 0. Transport errors (including [`LlmUnavailable`]) are returned as is.
    async fn chat_json<T>(
        &self,
        task: &str,
        lane: Lane,
        mut messages: Vec<Value>,
        max_tokens: u32,
        parse: impl Fn(&str) -> Option<T>,
    ) -> anyhow::Result<JsonReply<T>> {
        let first = self.chat(task, lane, &messages, max_tokens, 0.2).await?;
        if let Some(v) = parse(&first) {
            return Ok(JsonReply {
                parsed: Some(v),
                raw: first,
            });
        }
        messages.push(json!({"role": "assistant", "content": first}));
        messages.push(json!({"role": "user", "content": "Reply again with ONLY the JSON object described above."}));
        let second = self
            .chat(&format!("{task}-retry"), lane, &messages, max_tokens, 0.0)
            .await?;
        Ok(JsonReply {
            parsed: parse(&second),
            raw: second,
        })
    }

    /// Summarize with one format-reminder retry; falls back to the raw text (parse_ok = false).
    pub async fn summarize_email(
        &self,
        m: &ParsedMessage,
        language: &str,
        links: &[String],
        lane: Lane,
    ) -> anyhow::Result<EmailSummary> {
        let block = self.summary_block(m, links);
        let checked = |s: &EmailSummary| {
            let mut t = s.summary.clone();
            for a in &s.suggested_actions {
                t.push('\n');
                t.push_str(&a.text);
            }
            t
        };
        let reply = self
            .run(
                Task::Summary,
                lane,
                SUMMARY_TASK,
                language,
                &block,
                self.summary_max_tokens,
                |t| parse_summary(t, links),
                Some(&checked),
            )
            .await?;
        Ok(reply.parsed.unwrap_or_else(|| {
            tracing::warn!("LLM output was not valid JSON after retry; using raw text");
            let text: String = reply.raw.chars().take(1200).collect();
            EmailSummary {
                summary: if text.trim().is_empty() {
                    "(empty model output)".into()
                } else {
                    text
                },
                importance: medium(),
                category: other(),
                suggested_actions: vec![],
                language: String::new(),
                parse_ok: false,
            }
        }))
    }
}
