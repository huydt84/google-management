//! Morning digest (workstream 2A, ADR-0015).
//!
//! - `digest.item.summarize` (`digest_summarize`, batch lane): one line per digest item, or the job
//!   list of a job alert, stored in `events.summary`. Needs the model: deferred at night and during
//!   outages like every model task.
//! - `schedule.due{kind: digest}` (`digest_build`): one digest per user and local date
//!   (idempotent). Collects `digest` events not yet sent, groups them by rule section in rule order
//!   (the model's section last), sends one silent Telegram message (split at 4096 characters).
//!   Items without a summary yet are listed by subject and marked pending. Nothing is sent when
//!   there is nothing to say.
//! - `digest.refresh` (`digest_refresh`): when a pending item gets its summary the same day before
//!   model hours end, the sent message is edited.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde_json::{Value, json};

use crate::bus::{JobContext, Outcome};
use crate::db::Event;
use crate::gmail::message::ParsedMessage;
use crate::i18n::Tr;
use crate::llm::{Lane, LlmClient, parse_json_object};
use crate::notify::{Keyboard, OutgoingMessage, feedback_row, split_message};
use crate::pipeline::Pipeline;
use crate::rules::Verdict;
use crate::topics;
use crate::triage_model::MODEL_SECTION;

pub const DIGEST_TASK: &str = include_str!("../prompts/digest.md");
pub const JOBS_TASK: &str = include_str!("../prompts/jobs.md");
const LINE_MAX_TOKENS: u32 = 150;
const JOBS_MAX_TOKENS: u32 = 600;
const MAX_JOBS_PER_MAIL: usize = 10;
/// Feedback rows under a digest; further model items are rated on the dashboard.
pub const MAX_FEEDBACK_ROWS: usize = 8;
const JOBS_SECTION: &str = "jobs";

/// Current health problems, one localized line each (empty when healthy). Implemented by the health
/// checker (2E); the digest shows them in a "System" section, only while a check fails.
#[async_trait]
pub trait ProblemSource: Send + Sync {
    async fn problems(&self, tr: &Tr) -> Vec<String>;
}

/// The owner's current 👍/👎 per event (feedback, 2C), used to mark buttons on re-render.
#[async_trait]
pub trait FeedbackSource: Send + Sync {
    async fn labels(&self, event_ids: &[i64]) -> anyhow::Result<HashMap<i64, String>>;
}

pub struct Digest {
    pub calendar: Option<Arc<crate::calendar::Calendar>>,
    pub p: Arc<Pipeline>,
    pub problems: Option<Arc<dyn ProblemSource>>,
    pub feedback: Option<Arc<dyn FeedbackSource>>,
}

#[derive(serde::Deserialize)]
struct EventRef {
    event_id: i64,
}

#[derive(serde::Deserialize)]
pub struct ScheduleDue {
    pub kind: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub owner: Option<String>,
}

#[derive(serde::Deserialize)]
struct DigestRef {
    digest_id: i64,
}

// ------------------------------------------------------------------------------ item summaries

fn parse_line(text: &str) -> Option<String> {
    let line = parse_json_object(text)?
        .get("line")?
        .as_str()?
        .trim()
        .to_string();
    (!line.is_empty()).then_some(line)
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Job {
    pub title: String,
    #[serde(default)]
    pub company: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub link: String,
}

/// Parse the job list. A link is kept only if it is an https URL that appears verbatim in the mail
/// (the model must not be able to introduce links: invariant 2).
pub fn parse_jobs(text: &str, mail_text: &str) -> Option<Vec<Job>> {
    let obj = parse_json_object(text)?;
    let field = |j: &Value, k: &str| -> String {
        j.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .chars()
            .take(200)
            .collect()
    };
    let jobs = obj
        .get("jobs")?
        .as_array()?
        .iter()
        .filter_map(|j| {
            let title = field(j, "title");
            if title.is_empty() {
                return None;
            }
            let link = field(j, "link");
            Some(Job {
                title,
                company: field(j, "company"),
                location: field(j, "location"),
                link: if link.starts_with("https://") && mail_text.contains(&link) {
                    link
                } else {
                    String::new()
                },
            })
        })
        .take(MAX_JOBS_PER_MAIL)
        .collect();
    Some(jobs)
}

/// Summarize one digest item: `{line, parse_ok}`, or `{jobs, parse_ok}` for the jobs section.
/// Transport errors (including `LlmUnavailable`) are returned. Also used by `gm eval`.
pub async fn summarize_item(
    llm: &LlmClient,
    m: &ParsedMessage,
    section: &str,
    language: &str,
) -> anyhow::Result<Value> {
    let email = llm.email_block(m, usize::MAX);
    if section == JOBS_SECTION {
        let mail_text = format!("{}\n{}", m.body_text, m.snippet);
        let reply = llm
            .chat_json(
                "digest-jobs",
                Lane::Batch,
                llm.task_messages(JOBS_TASK, language, &email),
                JOBS_MAX_TOKENS,
                |t| parse_jobs(t, &mail_text),
            )
            .await?;
        return Ok(match reply.parsed {
            Some(jobs) => json!({"jobs": jobs, "parse_ok": true}),
            None => json!({"jobs": [], "parse_ok": false}),
        });
    }
    let reply = llm
        .chat_json(
            "digest-line",
            Lane::Batch,
            llm.task_messages(DIGEST_TASK, language, &email),
            LINE_MAX_TOKENS,
            parse_line,
        )
        .await?;
    Ok(match reply.parsed {
        Some(line) => json!({"line": line, "parse_ok": true}),
        None => {
            let raw: String = reply.raw.trim().chars().take(300).collect();
            if raw.is_empty() {
                json!({"parse_ok": false})
            } else {
                json!({"line": raw, "parse_ok": false})
            }
        }
    })
}

// ------------------------------------------------------------------------------ rendering

pub struct Rendered {
    pub text: String,
    pub buttons: Keyboard,
    pub items: usize,
    pub pending: usize,
    pub problems: usize,
    pub calendar_items: usize,
}

fn verdict(e: &Event) -> Option<Verdict> {
    Pipeline::verdict_of(&e.data)
}

fn is_pending(e: &Event) -> bool {
    e.status == "digest" && e.summary.is_none() && verdict(e).is_some_and(|v| v.summarize)
}

fn sender(e: &Event) -> String {
    let name = e.data["from_name"].as_str().unwrap_or_default();
    if name.is_empty() {
        e.data["from_addr"]
            .as_str()
            .or(e.data["from"].as_str())
            .unwrap_or("?")
            .to_string()
    } else {
        name.to_string()
    }
}

fn subject(e: &Event) -> String {
    e.data["subject"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("(no subject)")
        .to_string()
}

fn summary_line(e: &Event) -> Option<String> {
    e.summary
        .as_ref()?
        .get("line")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

fn item_line(e: &Event) -> String {
    let v = verdict(e).unwrap_or_else(|| Verdict {
        rule: String::new(),
        action: crate::rules::Action::Digest,
        priority: Default::default(),
        summarize: false,
        section: None,
        message: None,
        extracted: BTreeMap::new(),
        reason: None,
    });
    let text = if let Some(m) = v.message.filter(|m| !m.is_empty()) {
        m
    } else if let Some(line) = summary_line(e) {
        format!("{}: {line}", sender(e))
    } else if !v.extracted.is_empty() {
        let values: Vec<String> = v
            .extracted
            .iter()
            .map(|(k, val)| format!("{k}: {val}"))
            .collect();
        format!("{} ({})", subject(e), values.join(", "))
    } else {
        format!("{}: {}", sender(e), subject(e))
    };
    format!("• {}{text}", if is_pending(e) { "⏳ " } else { "" })
}

/// Render a digest. `order` is the rule section order ([`crate::rules::RuleSet::digest_sections`]);
/// sections not in it come next, the model's section last.
pub fn render(
    tr: &Tr,
    date: NaiveDate,
    events: &[Event],
    order: &[String],
    problems: &[String],
    feedback: &HashMap<i64, String>,
) -> Rendered {
    let mut by_section: BTreeMap<String, Vec<&Event>> = BTreeMap::new();
    for e in events {
        let section = verdict(e)
            .and_then(|v| v.section)
            .unwrap_or_else(|| "other".into());
        by_section.entry(section).or_default().push(e);
    }
    let mut sections: Vec<String> = order
        .iter()
        .filter(|s| s.as_str() != MODEL_SECTION)
        .cloned()
        .collect();
    for s in by_section.keys() {
        if !sections.contains(s) && s != MODEL_SECTION {
            sections.push(s.clone());
        }
    }
    sections.push(MODEL_SECTION.into());

    let mut lines = vec![tr.args(
        "digest-title",
        &[("date", date.format("%Y-%m-%d").to_string())],
    )];
    let mut buttons: Keyboard = Vec::new();
    for section in &sections {
        let Some(items) = by_section.get(section) else {
            continue;
        };
        lines.push(String::new());
        lines.push(tr.get_or(&format!("digest-section-{section}"), section));
        if section == JOBS_SECTION {
            let mut seen = HashSet::new();
            for e in items {
                let jobs: Vec<Job> = e
                    .summary
                    .as_ref()
                    .and_then(|s| serde_json::from_value(s["jobs"].clone()).ok())
                    .unwrap_or_default();
                if jobs.is_empty() {
                    lines.push(item_line(e));
                    continue;
                }
                for j in jobs {
                    let key = (j.title.to_lowercase(), j.company.to_lowercase());
                    if !seen.insert(key) {
                        continue;
                    }
                    let mut line = format!("• {}", j.title);
                    if !j.company.is_empty() {
                        line.push_str(&format!(" — {}", j.company));
                    }
                    if !j.location.is_empty() {
                        line.push_str(&format!(" · {}", j.location));
                    }
                    lines.push(line);
                    if !j.link.is_empty() {
                        lines.push(format!("  {}", j.link));
                    }
                }
            }
        } else if section == MODEL_SECTION {
            for (i, e) in items.iter().enumerate() {
                let n = i + 1;
                let body = match summary_line(e) {
                    Some(line) => format!("{}: {line}", sender(e)),
                    None => format!("{}: {}", sender(e), subject(e)),
                };
                lines.push(format!(
                    "[{n}] {}{body}",
                    if is_pending(e) { "⏳ " } else { "" }
                ));
                if let Some(reason) = verdict(e).and_then(|v| v.reason).filter(|r| !r.is_empty()) {
                    lines.push(format!("    ↳ {reason}"));
                }
                if i < MAX_FEEDBACK_ROWS {
                    buttons.push(feedback_row(
                        e.id,
                        &format!(" {n}"),
                        feedback.get(&e.id).map(String::as_str),
                    ));
                }
            }
        } else {
            for e in items {
                lines.push(item_line(e));
            }
        }
    }
    if !problems.is_empty() {
        lines.push(String::new());
        lines.push(tr.get("digest-system"));
        for p in problems {
            lines.push(format!("• {p}"));
        }
    }
    let pending = events.iter().filter(|e| is_pending(e)).count();
    if pending > 0 {
        lines.push(String::new());
        lines.push(tr.get("digest-pending-note"));
    }
    Rendered {
        text: lines.join("\n"),
        buttons,
        items: events.len(),
        pending,
        problems: problems.len(),
        calendar_items: 0,
    }
}

// ------------------------------------------------------------------------------ handlers

const EVENT_COLUMNS: &str =
    "id, account, source, kind, external_id, status, rule, data, summary, created_at";

impl Digest {
    pub fn register(self: &Arc<Self>) {
        let bus = &self.p.bus;
        let d = self.clone();
        bus.subscribe(
            topics::DIGEST_ITEM_SUMMARIZE,
            "digest_summarize",
            move |v, c| {
                let d = d.clone();
                async move { d.summarize(v, c).await }
            },
        );
        let d = self.clone();
        bus.subscribe(topics::SCHEDULE_DUE, "digest_build", move |v, c| {
            let d = d.clone();
            async move { d.on_schedule(v, c).await }
        });
        let d = self.clone();
        bus.subscribe(topics::DIGEST_REFRESH, "digest_refresh", move |v, c| {
            let d = d.clone();
            async move { d.refresh(v, c).await }
        });
    }

    pub async fn summarize(&self, payload: Value, _ctx: JobContext) -> anyhow::Result<Outcome> {
        let p = &self.p;
        let r: EventRef = serde_json::from_value(payload)?;
        let Some(ev) = crate::db::event(&p.pool, r.event_id).await? else {
            return Ok(Outcome::Done);
        };
        if ev.status != "digest" || ev.summary.is_some() {
            return Ok(Outcome::Done);
        }
        let Some(v) = verdict(&ev).filter(|v| v.summarize) else {
            return Ok(Outcome::Done);
        };
        let section = v.section.unwrap_or_default();
        let step = p
            .model_step(
                ev.id,
                "digest",
                &ev.account,
                &ev.external_id,
                "digest item",
                async |m, lang| summarize_item(&p.llm, m, &section, lang).await,
            )
            .await?;
        let summary = match step {
            Ok(s) => s,
            Err(outcome) => return Ok(outcome),
        };
        let mut tx = p.pool.begin().await?;
        let row: Option<(String, bool, Option<i64>)> = sqlx::query_as(
            "SELECT status, summary IS NULL, digest_id FROM events WHERE id = $1 FOR UPDATE",
        )
        .bind(ev.id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((status, unsummarized, digest_id)) = row else {
            return Ok(Outcome::Done);
        };
        if status != "digest" || !unsummarized {
            return Ok(Outcome::Done);
        }
        sqlx::query("UPDATE events SET summary = $2, updated_at = now() WHERE id = $1")
            .bind(ev.id)
            .bind(&summary)
            .execute(&mut *tx)
            .await?;
        if let Some(digest_id) = digest_id {
            let sent: Option<bool> =
                sqlx::query_scalar("SELECT status = 'sent' FROM digests WHERE id = $1")
                    .bind(digest_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if sent == Some(true) {
                p.bus
                    .publish(
                        &mut tx,
                        topics::DIGEST_REFRESH,
                        &json!({"digest_id": digest_id}),
                    )
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(Outcome::Done)
    }

    pub async fn on_schedule(&self, payload: Value, _ctx: JobContext) -> anyhow::Result<Outcome> {
        let due: ScheduleDue = serde_json::from_value(payload)?;
        match (due.kind.as_str(), due.owner) {
            ("digest", Some(owner)) => self.build(&owner, due.at).await,
            _ => Ok(Outcome::Done),
        }
    }

    /// Build and send the digest of `owner` for the local date of `at`. Idempotent per date: the
    /// row is created (and events claimed) once; a retry after a crash resends from that row.
    pub async fn build(&self, owner: &str, at: DateTime<Utc>) -> anyhow::Result<Outcome> {
        let pool = &self.p.pool;
        let date = at.with_timezone(&Local).date_naive();
        let mut tx = pool.begin().await?;
        let inserted: Option<i64> = sqlx::query_scalar(
            "INSERT INTO digests (owner_email, digest_date) VALUES ($1, $2)
             ON CONFLICT (owner_email, digest_date) DO NOTHING RETURNING id",
        )
        .bind(owner)
        .bind(date)
        .fetch_optional(&mut *tx)
        .await?;
        let (id, status) = match inserted {
            Some(id) => {
                // Items since the previous digest; the first digest covers the last 24 hours
                // (the first digest only takes the last 24 hours). Late-classified items of earlier days are still picked up.
                sqlx::query(
                    "UPDATE events SET digest_id = $1, updated_at = now()
                     WHERE status = 'digest' AND digest_id IS NULL
                       AND account IN (SELECT email FROM google_accounts WHERE owner_email = $2)
                       AND created_at >= COALESCE(
                           (SELECT min(created_at) FROM digests WHERE owner_email = $2 AND id <> $1),
                           now()) - interval '24 hours'",
                )
                .bind(id)
                .bind(owner)
                .execute(&mut *tx)
                .await?;
                (id, "building".to_string())
            }
            None => {
                sqlx::query_as::<_, (i64, String)>(
                    "SELECT id, status FROM digests WHERE owner_email = $1 AND digest_date = $2",
                )
                .bind(owner)
                .bind(date)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        tx.commit().await?;
        if status == "building" {
            self.send(id).await?;
        }
        Ok(Outcome::Done)
    }

    async fn events_of(&self, digest_id: i64) -> anyhow::Result<Vec<Event>> {
        Ok(sqlx::query_as::<_, Event>(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE digest_id = $1 ORDER BY id"
        ))
        .bind(digest_id)
        .fetch_all(&self.p.pool)
        .await?)
    }

    async fn render_digest(
        &self,
        owner: &str,
        date: NaiveDate,
        digest_id: i64,
    ) -> anyhow::Result<Rendered> {
        let prefs = self.p.user_settings.get(owner).await?;
        let tr = Tr::new(&prefs.ui_language);
        let events = self.events_of(digest_id).await?;
        let problems = match &self.problems {
            Some(src) => src.problems(&tr).await,
            None => vec![],
        };
        let model_ids: Vec<i64> = events
            .iter()
            .filter(|e| verdict(e).and_then(|v| v.section).as_deref() == Some(MODEL_SECTION))
            .map(|e| e.id)
            .collect();
        let feedback = match (&self.feedback, model_ids.is_empty()) {
            (Some(src), false) => src.labels(&model_ids).await?,
            _ => HashMap::new(),
        };
        let order = self.p.rules.get().digest_sections();
        let mut rendered = render(&tr, date, &events, &order, &problems, &feedback);
        if let Some(calendar) = &self.calendar {
            let agenda = calendar.agenda(owner, date, &tr).await?;
            rendered.calendar_items = agenda.len();
            if !agenda.is_empty() {
                let (heading, rest) = rendered
                    .text
                    .split_once('\n')
                    .unwrap_or((&rendered.text, ""));
                rendered.text = format!(
                    "{heading}\n\n{}\n{}{}",
                    tr.get("calendar-today"),
                    agenda.join("\n"),
                    rest
                );
            }
        }
        Ok(rendered)
    }

    async fn send(&self, digest_id: i64) -> anyhow::Result<()> {
        let pool = &self.p.pool;
        let (owner, date): (String, NaiveDate) =
            sqlx::query_as("SELECT owner_email, digest_date FROM digests WHERE id = $1")
                .bind(digest_id)
                .fetch_one(pool)
                .await?;
        let r = self.render_digest(&owner, date, digest_id).await?;
        if r.items == 0 && r.problems == 0 && r.calendar_items == 0 {
            // Nothing to say: no message; the run is still recorded.
            sqlx::query(
                "UPDATE digests SET status = 'empty', updated_at = now() WHERE id = $1 AND status = 'building'",
            )
            .bind(digest_id)
            .execute(pool)
            .await?;
            tracing::info!(digest_id, %owner, "digest empty; nothing sent");
            return Ok(());
        }
        let parts = split_message(&r.text);
        let mut ids = Vec::new();
        let last = parts.len() - 1;
        for (i, part) in parts.into_iter().enumerate() {
            let msg = OutgoingMessage {
                entities: vec![],
                text: part,
                silent: true, // nothing in the digest is urgent
                buttons: if i == last { r.buttons.clone() } else { vec![] },
                reply_to: None,
            };
            if let Some(id) = self.p.notifier.send(&msg).await? {
                ids.push(id);
            }
        }
        sqlx::query(
            "UPDATE digests SET status = 'sent', item_count = $2, message_ids = $3, text = $4,
                 sent_at = now(), updated_at = now() WHERE id = $1",
        )
        .bind(digest_id)
        .bind(r.items as i32)
        .bind(json!(ids))
        .bind(&r.text)
        .execute(pool)
        .await?;
        tracing::info!(digest_id, %owner, items = r.items, pending = r.pending, "digest sent");
        Ok(())
    }

    /// Edit a sent digest after late summaries, the same day and only during model hours.
    pub async fn refresh(&self, payload: Value, _ctx: JobContext) -> anyhow::Result<Outcome> {
        let r: DigestRef = serde_json::from_value(payload)?;
        let pool = &self.p.pool;
        let row: Option<(String, NaiveDate, String, Value, String)> = sqlx::query_as(
            "SELECT owner_email, digest_date, status, message_ids, text FROM digests WHERE id = $1",
        )
        .bind(r.digest_id)
        .fetch_optional(pool)
        .await?;
        let Some((owner, date, status, ids, old_text)) = row else {
            return Ok(Outcome::Done);
        };
        let now = self.p.clock.now();
        if status != "sent" || !self.p.model.hours().is_on(now) || now.date_naive() != date {
            return Ok(Outcome::Done);
        }
        let rendered = self.render_digest(&owner, date, r.digest_id).await?;
        if rendered.text == old_text {
            return Ok(Outcome::Done);
        }
        let mut ids: Vec<i64> = serde_json::from_value(ids).unwrap_or_default();
        let parts = split_message(&rendered.text);
        let last = parts.len() - 1;
        for (i, part) in parts.iter().enumerate() {
            let msg = OutgoingMessage {
                entities: vec![],
                text: part.clone(),
                silent: true,
                buttons: if i == last {
                    rendered.buttons.clone()
                } else {
                    vec![]
                },
                reply_to: None,
            };
            match ids.get(i) {
                Some(&id) => self.p.notifier.edit(id, &msg).await?,
                None => {
                    if let Some(id) = self.p.notifier.send(&msg).await? {
                        ids.push(id);
                    }
                }
            }
        }
        for &id in ids.iter().skip(parts.len()) {
            self.p
                .notifier
                .edit(id, &OutgoingMessage::text("·", true))
                .await?;
        }
        sqlx::query(
            "UPDATE digests SET text = $2, message_ids = $3, edited_at = now(), updated_at = now() WHERE id = $1",
        )
        .bind(r.digest_id)
        .bind(&rendered.text)
        .bind(json!(ids))
        .execute(pool)
        .await?;
        Ok(Outcome::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_links_must_come_from_the_mail() {
        let out = r#"{"jobs": [
            {"title": "Engineer", "company": "A", "location": "Tokyo", "link": "https://www.linkedin.com/jobs/view/1"},
            {"title": "Evil", "company": "B", "link": "https://phish.example/x"},
            {"title": "", "company": "C"}]}"#;
        let jobs = parse_jobs(out, "see https://www.linkedin.com/jobs/view/1 now").unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].link, "https://www.linkedin.com/jobs/view/1");
        assert_eq!(jobs[1].link, "", "a link not in the mail is dropped");
    }
}
