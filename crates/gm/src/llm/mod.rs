//! All model calls go through [`LlmClient`] (invariant 7, ADR-0010): an OpenAI-compatible
//! `POST /chat/completions` to mlx-server-runtime, admitted through priority lanes, recorded in
//! `llm_calls`. No `tools` / `response_format`: prompts ask for one JSON object and the output is
//! parsed tolerantly. A model client defines how each task tier prompts, reasons and retries.
//! The sampling profile controls the initial temperature and top-p.

pub mod clean;
pub mod language;
pub(crate) mod model_client;
pub(crate) mod profile;

pub use model_client::Task;

use model_client::{ClientRegistry, Mode};
use profile::{Profiles, Sampling};

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

/// A task that answers in the owner's language gets it restated after the mail, since a thinking
/// model tends to answer in the language it reasoned in.
fn task_messages(
    task: Task,
    system_suffix: &str,
    prompt: &str,
    language: &str,
    email_block: &str,
) -> Vec<Value> {
    let mut system = format!("{}\n{}", SYSTEM_PREFIX.trim_end(), prompt.trim_end());
    if !system_suffix.is_empty() {
        system.push('\n');
        system.push_str(system_suffix);
    }
    let mut user = format!("Output language: {language}\n\n{email_block}");
    if task.in_owner_language() {
        user.push_str(&format!(
            "\n\nOutput language: {language}. Write every value in the JSON in {language}."
        ));
    }
    vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": user}),
    ]
}

pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    pub model: String,
    profiles: Profiles,
    clients: ClientRegistry,
    /// Models already reported as running on the built-in defaults.
    warned: Mutex<std::collections::BTreeSet<String>>,
    max_input_tokens: usize,
    summary_max_tokens: u32,
    admission: Arc<Admission>,
    pool: Option<PgPool>,
    recorder: Option<Arc<Mutex<Vec<CallRecord>>>>,
    control: Option<Arc<crate::model::ModelControl>>,
}

/// Keep backend identity, sampling and residency consistent through all attempts at a task.
struct ModelRequest {
    model: String,
    lane: Lane,
    sampling: Sampling,
    _running: Option<crate::model::RequestGuard>,
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
    /// Loads sampling profiles (`GM_LLM_PROFILE`). An invalid file is an error.
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
            clients: ClientRegistry::load()?,
            warned: Mutex::new(Default::default()),
            max_input_tokens: cfg.llm_max_input_tokens,
            summary_max_tokens: cfg.llm_summary_max_tokens,
            admission: Admission::new(cfg.llm_max_inflight),
            pool,
            recorder: None,
            control: None,
        })
    }

    async fn begin(&self, lane: Lane) -> anyhow::Result<ModelRequest> {
        let running = match &self.control {
            Some(control) => Some(control.begin(lane).await?),
            None => None,
        };
        let model = self.request_model();
        let key = self.clients.profile_key(&model);
        if self.warned.lock().unwrap().insert(model.clone()) {
            let (detail, warning) = self
                .profiles
                .describe(key, &self.clients.client_name(&model));
            match warning {
                Some(reason) => tracing::warn!(reason, profile = detail, "LLM sampling profile"),
                None => tracing::info!(profile = detail, "LLM sampling profile"),
            }
        }
        Ok(ModelRequest {
            sampling: self.profiles.sampling(key),
            model,
            lane,
            _running: running,
        })
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
        let request = self.begin(lane).await?;
        self.complete(&request, task, messages, max_tokens, temperature)
            .await
    }

    async fn complete(
        &self,
        request: &ModelRequest,
        task: &str,
        messages: &[Value],
        max_tokens: u32,
        temperature: f32,
    ) -> anyhow::Result<String> {
        let model = &request.model;
        let lane = request.lane;
        let _permit = self.admission.acquire(lane).await;
        let started = Instant::now();
        let mut body = json!({
            "model": model, "messages": messages, "max_tokens": max_tokens,
            "temperature": temperature, "stream": false
        });
        if let Some(top_p) = request.sampling.top_p {
            body["top_p"] = json!(top_p);
        }
        let result = async {
            let resp = self
                .http
                .post(format!("{}/chat/completions", self.base_url))
                .bearer_auth(&self.api_key)
                .json(&body)
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
            .bind(model)
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

    /// Preview the first request using the current model's inference plan.
    pub fn messages_for(
        &self,
        task: Task,
        prompt: &str,
        language: &str,
        email_block: &str,
    ) -> Vec<Value> {
        let inference = self.clients.inference_for(&self.request_model(), task);
        task_messages(
            task,
            inference.system_suffix(),
            prompt,
            language,
            email_block,
        )
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

    /// Run a task with the active model's inference plan. `checked` extracts the answer text
    /// for clients that retry when the answer is in a different language.
    #[allow(clippy::too_many_arguments)]
    pub async fn run<T>(
        &self,
        task: Task,
        lane: Lane,
        prompt: &str,
        language: &str,
        block: &str,
        parse: impl Fn(&str) -> Option<T>,
        checked: Option<&(dyn Fn(&T) -> String + Sync)>,
    ) -> anyhow::Result<JsonReply<T>> {
        let request = self.begin(lane).await?;
        let inference = self.clients.inference_for(&request.model, task);
        let name = task.name();
        let messages = task_messages(task, inference.system_suffix(), prompt, language, block);
        let answer_cap = inference.answer.resolve(self.summary_max_tokens);
        let Mode::Thinking {
            reasoning_tokens,
            decode_answer,
            fallback_tag,
            language_check,
        } = inference.mode
        else {
            return self
                .chat_json(&request, name, messages, answer_cap, parse)
                .await;
        };

        let max_tokens = answer_cap
            .checked_add(reasoning_tokens)
            .ok_or_else(|| anyhow::anyhow!("completion token budget overflow for {name}"))?;
        let first = self
            .complete(
                &request,
                name,
                &messages,
                max_tokens,
                request.sampling.thinking_temperature(),
            )
            .await?;
        let first = decode_answer(first);
        let parsed = parse(&first);
        let wrong_language = language_check
            && parsed.as_ref().is_some_and(|value| {
                checked.is_some_and(|check| {
                    language::in_language(language, &check(value)) == Some(false)
                })
            });
        if parsed.is_some() && !wrong_language {
            return Ok(JsonReply { parsed, raw: first });
        }
        tracing::warn!(task = name, wrong_language, "retrying model answer");
        match fallback_tag {
            Some(fallback_tag) => {
                let messages = task_messages(task, fallback_tag, prompt, language, block);
                self.chat_json(
                    &request,
                    &format!("{name}-nothink"),
                    messages,
                    answer_cap,
                    parse,
                )
                .await
            }
            None => {
                let raw = self
                    .complete(
                        &request,
                        &format!("{name}-retry"),
                        &messages,
                        max_tokens,
                        0.0,
                    )
                    .await?;
                let raw = decode_answer(raw);
                let retried = parse(&raw);
                Ok(if retried.is_none() && parsed.is_some() {
                    JsonReply { parsed, raw: first }
                } else {
                    JsonReply {
                        parsed: retried,
                        raw,
                    }
                })
            }
        }
    }

    /// Ask for one JSON object; if `parse` rejects the reply, ask once more with a format reminder
    /// at temperature 0. Transport errors (including [`LlmUnavailable`]) are returned as is.
    async fn chat_json<T>(
        &self,
        request: &ModelRequest,
        task: &str,
        mut messages: Vec<Value>,
        max_tokens: u32,
        parse: impl Fn(&str) -> Option<T>,
    ) -> anyhow::Result<JsonReply<T>> {
        let first = self
            .complete(
                request,
                task,
                &messages,
                max_tokens,
                request.sampling.direct_temperature(),
            )
            .await?;
        if let Some(v) = parse(&first) {
            return Ok(JsonReply {
                parsed: Some(v),
                raw: first,
            });
        }
        messages.push(json!({"role": "assistant", "content": first}));
        messages.push(json!({"role": "user", "content": "Reply again with ONLY the JSON object described above."}));
        let second = self
            .complete(
                request,
                &format!("{task}-retry"),
                &messages,
                max_tokens,
                0.0,
            )
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
