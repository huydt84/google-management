//! Local eval set and `gm eval` (workstream 2F, ADR-0017).
//!
//! Cases are plain JSON files `<gmail id>.json` in `GM_EVAL_DIR` (default
//! `~/.local/share/gm/eval/`, mode 0700, files 0600), at most 200: the raw Gmail message, the
//! expected action and optional notes. They are added from the dashboard, kept until removed
//! (outside the database, so retention doesn't touch them), never committed and never sent to any
//! external service: `gm eval` refuses a model endpoint that isn't on this machine.
//!
//! `gm eval` runs rules → model triage (for unmatched mail) → summary (notification summary or digest
//! item) on every case and reports accuracy per expected action, the confusion matrix, the JSON
//! parse rate per task, and latency p50/p95 and tokens per model task.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::gmail::message::parse_message;
use crate::llm::{CallRecord, Lane, LlmClient};
use crate::pipeline::Pipeline;
use crate::rules::{Action, RuleSet};
use crate::topics;

pub const MAX_CASES: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalCase {
    pub message_id: String,
    pub account: String,
    pub added_at: DateTime<Utc>,
    pub expected: Action,
    #[serde(default)]
    pub notes: String,
    /// The Gmail API message (`format=full`) as fetched once when added.
    pub message: Value,
}

#[derive(Debug, PartialEq)]
pub enum Added {
    New,
    Updated,
    Full,
}

pub fn case_path(dir: &Path, message_id: &str) -> PathBuf {
    // Gmail ids are hex; keep only safe characters for the file name.
    let safe: String = message_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    dir.join(format!("{safe}.json"))
}

fn case_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    Ok(files)
}

/// Message ids currently in the set.
pub fn ids(dir: &Path) -> Vec<String> {
    case_files(dir)
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect()
}

/// Add a case, or update the expected action of an existing one. At most [`MAX_CASES`].
pub fn add(dir: &Path, case: &EvalCase) -> anyhow::Result<Added> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let path = case_path(dir, &case.message_id);
    let existed = path.exists();
    if !existed && case_files(dir)?.len() >= MAX_CASES {
        return Ok(Added::Full);
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(case)?)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, &path)?;
    Ok(if existed { Added::Updated } else { Added::New })
}

pub fn load(dir: &Path) -> anyhow::Result<Vec<EvalCase>> {
    let mut cases = Vec::new();
    for p in case_files(dir)? {
        let text = std::fs::read_to_string(&p)?;
        match serde_json::from_str::<EvalCase>(&text) {
            Ok(c) if c.expected != Action::Triage => cases.push(c),
            Ok(_) => eprintln!(
                "skipping {}: expected must be notify, digest or ignore",
                p.display()
            ),
            Err(e) => eprintln!("skipping {}: {e}", p.display()),
        }
    }
    Ok(cases)
}

/// `gm eval` talks to the local model only (ADR-0017): the eval set never leaves the machine.
pub fn ensure_local(base_url: &str) -> anyhow::Result<()> {
    let host = url::Url::parse(base_url)?
        .host_str()
        .unwrap_or_default()
        .to_string();
    anyhow::ensure!(
        matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1"),
        "GM_LLM_BASE_URL must point at this machine for gm eval (got host {host:?}): the eval set must not leave the Mac"
    );
    Ok(())
}

#[derive(Debug, Default)]
pub struct Report {
    pub cases: usize,
    /// (expected, got) → count.
    pub confusion: BTreeMap<(&'static str, &'static str), usize>,
    /// task → (parsed, total).
    pub parse: BTreeMap<&'static str, (usize, usize)>,
    pub calls: Vec<CallRecord>,
    /// Message ids whose decision differed from the expected action.
    pub wrong: Vec<(String, &'static str, &'static str)>,
}

impl Report {
    fn parsed(&mut self, task: &'static str, ok: bool) {
        let e = self.parse.entry(task).or_default();
        e.0 += usize::from(ok);
        e.1 += 1;
    }
}

/// Run the pipeline's decisions on every case. `llm` must have been set up with
/// [`LlmClient::record_calls`]; pass that list as `calls`.
pub async fn run(
    llm: &LlmClient,
    calls: &std::sync::Mutex<Vec<CallRecord>>,
    rules: &RuleSet,
    cases: &[EvalCase],
    language: &str,
) -> anyhow::Result<Report> {
    let mut report = Report {
        cases: cases.len(),
        ..Default::default()
    };
    for case in cases {
        let msg = parse_message(&case.message);
        let mut verdict = rules.evaluate(&msg);
        if verdict.action == Action::Triage {
            let d = crate::triage_model::classify(llm, &msg, language).await?;
            report.parsed("triage", d.parse_ok);
            verdict = d.verdict();
        }
        let (expected, got) = (case.expected.as_str(), verdict.action.as_str());
        *report.confusion.entry((expected, got)).or_default() += 1;
        if expected != got {
            report.wrong.push((case.message_id.clone(), expected, got));
        }
        // The summary the pipeline would run next for this verdict.
        match Pipeline::route(&verdict).1 {
            Some(topics::GMAIL_MESSAGE_SUMMARIZE) => {
                let s = llm.summarize_email(&msg, language, Lane::Notify).await?;
                report.parsed("summary", s.parse_ok);
            }
            Some(topics::DIGEST_ITEM_SUMMARIZE) => {
                let section = verdict.section.clone().unwrap_or_default();
                let s = crate::digest::summarize_item(llm, &msg, &section, language).await?;
                report.parsed("digest", s["parse_ok"].as_bool().unwrap_or(false));
            }
            _ => {}
        }
    }
    report.calls = calls.lock().unwrap().clone();
    Ok(report)
}

fn pct(n: usize, d: usize) -> String {
    if d == 0 {
        "—".into()
    } else {
        format!("{n}/{d} ({:.0}%)", 100.0 * n as f64 / d as f64)
    }
}

fn percentile(sorted: &[i32], p: f64) -> i32 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[idx]
}

pub fn format_report(r: &Report, model: &str) -> String {
    let mut out = format!(
        "gm eval: {} cases, model {model}\n\nAccuracy by expected action\n",
        r.cases
    );
    let (mut ok_all, mut n_all) = (0, 0);
    for a in Action::FINAL {
        let e = a.as_str();
        let n: usize = r
            .confusion
            .iter()
            .filter(|((x, _), _)| *x == e)
            .map(|(_, c)| c)
            .sum();
        let ok = r.confusion.get(&(e, e)).copied().unwrap_or(0);
        ok_all += ok;
        n_all += n;
        out.push_str(&format!("  {e:<8} {}\n", pct(ok, n)));
    }
    out.push_str(&format!("  {:<8} {}\n", "overall", pct(ok_all, n_all)));
    out.push_str("\nConfusion (rows: expected, columns: got)\n          notify digest ignore\n");
    for a in Action::FINAL {
        let e = a.as_str();
        out.push_str(&format!("  {e:<8}"));
        for g in Action::FINAL {
            let c = r.confusion.get(&(e, g.as_str())).copied().unwrap_or(0);
            out.push_str(&format!(" {c:>6}"));
        }
        out.push('\n');
    }
    out.push_str("\nJSON parse rate (after the retry)\n");
    for task in ["triage", "summary", "digest"] {
        let (ok, n) = r.parse.get(task).copied().unwrap_or((0, 0));
        out.push_str(&format!("  {task:<8} {}\n", pct(ok, n)));
    }
    out.push_str(
        "\nModel calls\n  task              calls  p50 ms  p95 ms  prompt tok  completion tok\n",
    );
    let mut by_task: BTreeMap<&str, Vec<&CallRecord>> = BTreeMap::new();
    for c in &r.calls {
        by_task.entry(c.task.as_str()).or_default().push(c);
    }
    for (task, calls) in by_task {
        let mut lat: Vec<i32> = calls.iter().map(|c| c.latency_ms).collect();
        lat.sort_unstable();
        let pt: i64 = calls
            .iter()
            .filter_map(|c| c.prompt_tokens)
            .map(i64::from)
            .sum();
        let ct: i64 = calls
            .iter()
            .filter_map(|c| c.completion_tokens)
            .map(i64::from)
            .sum();
        out.push_str(&format!(
            "  {task:<16} {:>6} {:>7} {:>7} {pt:>11} {ct:>15}\n",
            calls.len(),
            percentile(&lat, 0.5),
            percentile(&lat, 0.95)
        ));
    }
    if !r.wrong.is_empty() {
        out.push_str("\nMisclassified\n");
        for (id, e, g) in &r.wrong {
            out.push_str(&format!("  {id}: expected {e}, got {g}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_endpoints() {
        assert!(ensure_local("http://127.0.0.1:8000/v1").is_ok());
        assert!(ensure_local("http://localhost:8000/v1").is_ok());
        assert!(ensure_local("https://api.example.com/v1").is_err());
    }

    #[test]
    fn percentiles() {
        let v = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&v, 0.5), 50);
        assert_eq!(percentile(&v, 0.95), 100);
        assert_eq!(percentile(&[], 0.5), 0);
    }
}
