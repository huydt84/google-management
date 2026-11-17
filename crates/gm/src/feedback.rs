//! Triage feedback (workstream 2C). 👍 / 👎 under notifications and
//! model-triaged digest items (`f:<event_id>:up|down`), or on the dashboard. Stored in
//! `triage_feedback`; last tap wins. After a Telegram tap the keyboard is redrawn with ✅ on the
//! current choice. 👎 on a rule-matched mail points at the rule to edit; on a model-triaged mail it is
//! an eval candidate (2F). No mute button: rules stay in `rules.yaml`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::Value;
use sqlx::PgPool;

use crate::bus::{JobContext, Outcome};
use crate::notify::{Button, Keyboard, Notifier};
use crate::topics;

static DATA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^f:(\d{1,18}):(up|down)$").unwrap());

/// Parse feedback callback data strictly: `(event_id, label)`.
pub fn parse_data(data: &str) -> Option<(i64, &'static str)> {
    let c = DATA_RE.captures(data)?;
    let id = c[1].parse().ok()?;
    Some((id, if &c[2] == "up" { "up" } else { "down" }))
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct FeedbackReceived {
    pub event_id: i64,
    pub label: String,
    pub tapped_at: DateTime<Utc>,
    /// Telegram message whose keyboard to redraw, and that keyboard as shown.
    #[serde(default)]
    pub message_id: Option<i64>,
    #[serde(default)]
    pub keyboard: Keyboard,
}

pub struct Feedback {
    pub pool: PgPool,
    pub notifier: Arc<dyn Notifier>,
}

/// Store a 👍/👎 unless a newer one exists. Returns false if the event is gone.
pub async fn record(
    pool: &PgPool,
    event_id: i64,
    label: &str,
    via: &str,
    tapped_at: DateTime<Utc>,
) -> anyhow::Result<bool> {
    anyhow::ensure!(matches!(label, "up" | "down"), "invalid label");
    let r = sqlx::query(
        "INSERT INTO triage_feedback (event_id, label, rule, via, tapped_at)
         SELECT id, $2, rule, $3, $4 FROM events WHERE id = $1 AND source = 'gmail'
         ON CONFLICT (event_id) DO UPDATE SET label = EXCLUDED.label, via = EXCLUDED.via,
             tapped_at = EXCLUDED.tapped_at, updated_at = now()
         WHERE triage_feedback.tapped_at <= EXCLUDED.tapped_at",
    )
    .bind(event_id)
    .bind(label)
    .bind(via)
    .bind(tapped_at)
    .execute(pool)
    .await?;
    if r.rows_affected() == 0 {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM events WHERE id = $1 AND source = 'gmail')",
        )
        .bind(event_id)
        .fetch_one(pool)
        .await?;
        return Ok(exists);
    }
    Ok(true)
}

pub async fn labels(pool: &PgPool, event_ids: &[i64]) -> anyhow::Result<HashMap<i64, String>> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT event_id, label FROM triage_feedback WHERE event_id = ANY($1)")
            .bind(event_ids)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

/// Redraw feedback buttons with ✅ on each event's current label; other buttons stay as they are.
pub fn mark(keyboard: &Keyboard, current: &HashMap<i64, String>) -> Keyboard {
    keyboard
        .iter()
        .map(|row| {
            row.iter()
                .map(|b| match parse_data(&b.data) {
                    Some((id, label)) => {
                        let base = b.text.trim_start_matches('✅');
                        let chosen = current.get(&id).map(String::as_str) == Some(label);
                        Button {
                            text: format!("{}{base}", if chosen { "✅" } else { "" }),
                            data: b.data.clone(),
                        }
                    }
                    None => b.clone(),
                })
                .collect()
        })
        .collect()
}

impl Feedback {
    pub fn register(self: &Arc<Self>, bus: &crate::bus::Bus) {
        let f = self.clone();
        bus.subscribe(topics::FEEDBACK_RECEIVED, "feedback", move |v, c| {
            let f = f.clone();
            async move { f.handle(v, c).await }
        });
    }

    pub async fn handle(&self, payload: Value, _ctx: JobContext) -> anyhow::Result<Outcome> {
        let fb: FeedbackReceived = serde_json::from_value(payload)?;
        if !record(&self.pool, fb.event_id, &fb.label, "telegram", fb.tapped_at).await? {
            return Ok(Outcome::Done); // event deleted (retention)
        }
        tracing::info!(event_id = fb.event_id, label = %fb.label, "feedback recorded");
        if let Some(message_id) = fb.message_id.filter(|_| !fb.keyboard.is_empty()) {
            let ids: Vec<i64> = fb
                .keyboard
                .iter()
                .flatten()
                .filter_map(|b| parse_data(&b.data).map(|(id, _)| id))
                .collect();
            let current = labels(&self.pool, &ids).await?;
            self.notifier
                .edit_buttons(message_id, &mark(&fb.keyboard, &current))
                .await?;
        }
        Ok(Outcome::Done)
    }
}

#[async_trait]
impl crate::digest::FeedbackSource for Feedback {
    async fn labels(&self, event_ids: &[i64]) -> anyhow::Result<HashMap<i64, String>> {
        labels(&self.pool, event_ids).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_is_parsed_strictly() {
        assert_eq!(parse_data("f:12:up"), Some((12, "up")));
        assert_eq!(parse_data("f:12:down"), Some((12, "down")));
        for bad in [
            "f:12:sideways",
            "f:x:up",
            "f:12:up:extra",
            "a:1:y",
            "f:1234567890123456789:up",
        ] {
            assert_eq!(parse_data(bad), None, "{bad}");
        }
    }

    #[test]
    fn marks_follow_the_current_label() {
        let kb = vec![crate::notify::feedback_row(5, " 1", Some("up"))];
        let current = HashMap::from([(5, "down".to_string())]);
        let marked = mark(&kb, &current);
        assert_eq!(marked[0][0].text, "👍 1");
        assert_eq!(marked[0][1].text, "✅👎 1");
    }
}
