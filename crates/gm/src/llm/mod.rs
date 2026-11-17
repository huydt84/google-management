//! All model calls go through [`LlmClient`] (invariant 7, ADR-0010): an OpenAI-compatible
//! `POST /chat/completions` to mlx-server-runtime, admitted through priority lanes, recorded in
//! `llm_calls`. No `tools` / `response_format`: prompts ask for one JSON object and the output is
//! parsed tolerantly, with one retry.

pub mod clean;

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
/// (decision L12).
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
    pub suggested_actions: Vec<String>,
    #[serde(default)]
    pub language: String,
    #[serde(default = "yes")]
    pub parse_ok: bool,
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

/// Strip reasoning and fences and parse the outermost `{...}` as a JSON object. `None` if there is
/// none. Every task parses model output through this (L6: no JSON mode, tolerant parse).
pub fn parse_json_object(text: &str) -> Option<serde_json::Map<String, Value>> {
    static THINK: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?is)<think>.*?</think>").unwrap());
    let cleaned = THINK.replace_all(text, "");
    let (start, end) = (cleaned.find('{')?, cleaned.rfind('}')?);
    if end <= start {
        return None;
    }
    match serde_json::from_str(&cleaned[start..=end]).ok()? {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

/// Validate a summary. `None` if unusable.
pub fn parse_summary(text: &str) -> Option<EmailSummary> {
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
                    .filter_map(|x| x.as_str().map(str::to_string))
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

pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    pub model: String,
    no_think_tag: String,
    max_input_tokens: usize,
    summary_max_tokens: u32,
    admission: Arc<Admission>,
    pool: Option<PgPool>,
    recorder: Option<Arc<Mutex<Vec<CallRecord>>>>,
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
    pub fn new(cfg: &Config, pool: Option<PgPool>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(cfg.llm_timeout_s))
                .build()
                .expect("reqwest client"),
            base_url: cfg.llm_base_url.clone(),
            api_key: cfg.llm_api_key.clone(),
            model: cfg.llm_model.clone(),
            no_think_tag: cfg.llm_no_think_tag.clone(),
            max_input_tokens: cfg.llm_max_input_tokens,
            summary_max_tokens: cfg.llm_summary_max_tokens,
            admission: Admission::new(cfg.llm_max_inflight),
            pool,
            recorder: None,
        }
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
        let _permit = self.admission.acquire(lane).await;
        let started = Instant::now();
        let result = async {
            let resp = self
                .http
                .post(format!("{}/chat/completions", self.base_url))
                .bearer_auth(&self.api_key)
                .json(&json!({
                    "model": self.model, "messages": messages, "max_tokens": max_tokens,
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
            .bind(&self.model)
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
    /// delimiters neutralized and wrapped in `<email>` (invariant 2, L8).
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

    /// Shared stable prefix first (system prompt + task + thinking switch), per-user settings and
    /// the mail last, so the prefix KV cache is reused across mails and tasks (L9).
    pub fn task_messages(&self, task: &str, language: &str, email_block: &str) -> Vec<Value> {
        let mut system = format!("{}\n{}", SYSTEM_PREFIX.trim_end(), task.trim_end());
        if !self.no_think_tag.is_empty() {
            system.push_str(&format!("\n{}", self.no_think_tag));
        }
        vec![
            json!({"role": "system", "content": system}),
            json!({"role": "user", "content": format!("Output language: {language}\n\n{email_block}")}),
        ]
    }

    pub fn summary_messages(&self, m: &ParsedMessage, language: &str) -> Vec<Value> {
        self.task_messages(
            SUMMARY_TASK,
            language,
            &self.email_block(m, self.max_input_tokens),
        )
    }

    /// Ask for one JSON object; if `parse` rejects the reply, ask once more with a format reminder
    /// at temperature 0 (L6). Transport errors (including [`LlmUnavailable`]) are returned as is.
    pub async fn chat_json<T>(
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
        lane: Lane,
    ) -> anyhow::Result<EmailSummary> {
        let reply = self
            .chat_json(
                "summary",
                lane,
                self.summary_messages(m, language),
                self.summary_max_tokens,
                parse_summary,
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
