//! Model triage (workstream 2B, decision T16): mail no rule covers is classified by the model as
//! notify / digest / ignore with a short reason. Notify lane, `/no_think`, short output. Invalid
//! output after the retry fails open to `notify`, so nothing important is lost.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::gmail::message::ParsedMessage;
use crate::llm::{Lane, LlmClient, parse_json_object};
use crate::rules::{Action, Priority, Verdict};

pub const TRIAGE_TASK: &str = include_str!("../prompts/triage.md");
/// Output cap: one action and a reason of at most 12 words.
pub const MAX_OUTPUT_TOKENS: u32 = 80;
/// Only the beginning of the body is needed to decide.
pub const MAX_INPUT_TOKENS: usize = 1500;
/// Rule name recorded for decisions made by the model.
pub const MODEL_RULE: &str = "model";
/// Digest section for model-triaged mail (decision N6): rendered last, as "Other mail".
pub const MODEL_SECTION: &str = "model";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriageDecision {
    pub action: Action,
    pub reason: String,
    /// False when the model output was unusable and the decision is the fail-open default.
    pub parse_ok: bool,
}

pub fn parse_triage(text: &str) -> Option<TriageDecision> {
    let obj = parse_json_object(text)?;
    let action = Action::parse_final(obj.get("action")?.as_str()?)?;
    let reason: String = obj
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .chars()
        .take(200)
        .collect();
    Some(TriageDecision {
        action,
        reason,
        parse_ok: true,
    })
}

pub fn messages(llm: &LlmClient, m: &ParsedMessage, language: &str) -> Vec<Value> {
    llm.task_messages(TRIAGE_TASK, language, &llm.email_block(m, MAX_INPUT_TOKENS))
}

/// Classify one mail. Transport errors (including `LlmUnavailable`) are returned; unusable output
/// becomes `notify` with `parse_ok = false`.
pub async fn classify(
    llm: &LlmClient,
    m: &ParsedMessage,
    language: &str,
) -> anyhow::Result<TriageDecision> {
    let reply = llm
        .chat_json(
            "triage",
            Lane::Notify,
            messages(llm, m, language),
            MAX_OUTPUT_TOKENS,
            parse_triage,
        )
        .await?;
    Ok(reply.parsed.unwrap_or_else(|| {
        tracing::warn!("model triage output unusable after retry; failing open to notify");
        TriageDecision {
            action: Action::Notify,
            reason: String::new(),
            parse_ok: false,
        }
    }))
}

impl TriageDecision {
    /// The verdict recorded on the event, continued as if a rule had said this action.
    pub fn verdict(&self) -> Verdict {
        Verdict {
            rule: MODEL_RULE.into(),
            action: self.action,
            priority: Priority::Normal,
            summarize: self.action != Action::Ignore,
            section: (self.action == Action::Digest).then(|| MODEL_SECTION.into()),
            message: None,
            extracted: BTreeMap::new(),
            reason: Some(if self.parse_ok {
                self.reason.clone()
            } else {
                "model output unusable; notified to be safe".into()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_tolerant_and_strict_on_action() {
        let d = parse_triage(
            "<think></think>```json\n{\"action\": \"Digest\", \"reason\": \"receipt\"}\n```",
        )
        .unwrap();
        assert_eq!(d.action, Action::Digest);
        assert_eq!(d.reason, "receipt");
        assert!(parse_triage("{\"action\": \"archive\"}").is_none());
        assert!(parse_triage("notify").is_none());
        let v = d.verdict();
        assert_eq!(v.rule, "model");
        assert_eq!(v.section.as_deref(), Some("model"));
        assert!(v.summarize);
    }
}
