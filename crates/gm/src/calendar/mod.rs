//! Primary-calendar polling, template notifications and the digest's live agenda.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::bus::{JobContext, Outcome};
use crate::google::{CALENDAR_READONLY, GoogleError, calendar::CalendarClient};
use crate::i18n::Tr;
use crate::notify::OutgoingMessage;
use crate::pipeline::Pipeline;
use crate::topics;

mod message;
pub(crate) use message::{calendar_link, notification};

const PRIMARY: &str = "primary";
const WINDOW_DAYS: i64 = 30;

pub struct Calendar {
    pub p: Arc<Pipeline>,
    pub api: CalendarClient,
}

impl Calendar {
    pub fn register(self: &Arc<Self>) {
        for (topic, subscriber) in [
            (
                topics::CALENDAR_EVENT_INVITATION,
                "notify_calendar_invitation",
            ),
            (topics::CALENDAR_EVENT_CHANGED, "notify_calendar_changed"),
            (
                topics::CALENDAR_EVENT_CANCELLED,
                "notify_calendar_cancelled",
            ),
        ] {
            let calendar = self.clone();
            self.p.bus.subscribe(topic, subscriber, move |v, c| {
                let calendar = calendar.clone();
                async move { calendar.notify(v, c).await }
            });
        }
    }

    async fn connected(&self, email: &str) -> Result<bool, GoogleError> {
        Ok(self
            .p
            .accounts
            .get(email)
            .await
            .map_err(anyhow::Error::from)?
            .is_some_and(|a| {
                a.status == "active" && a.scopes.0.iter().any(|s| s == CALENDAR_READONLY)
            }))
    }

    /// Read every page before returning a new cursor. A partial or malformed sync never commits.
    async fn changes(
        &self,
        email: &str,
        token: Option<&str>,
    ) -> Result<(Vec<Value>, String), GoogleError> {
        let mut query = vec![
            ("singleEvents", "false".into()),
            ("showDeleted", "true".into()),
            ("maxResults", "2500".into()),
        ];
        if let Some(token) = token {
            query.push(("syncToken", token.into()));
        }
        let page = self.api.events(email, PRIMARY, &query).await?;
        Ok((
            page.items,
            page.next_sync_token
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Calendar response missing nextSyncToken"))?,
        ))
    }

    /// Live, expanded instances. Time filters are never combined with a sync token.
    async fn between(
        &self,
        email: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Value>, GoogleError> {
        let query = vec![
            ("singleEvents", "true".into()),
            ("orderBy", "startTime".into()),
            ("timeMin", start.to_rfc3339()),
            ("timeMax", end.to_rfc3339()),
            ("maxResults", "2500".into()),
        ];
        Ok(self
            .api
            .events(email, PRIMARY, &query)
            .await?
            .items
            .into_iter()
            .filter(|e| !declined(e) && e["status"] != "cancelled")
            .collect())
    }

    pub async fn sync(&self, email: &str) -> Result<usize, GoogleError> {
        if !self.connected(email).await? {
            return Ok(0);
        }
        let result = self.sync_locked(email).await;
        if let Err(ref error) = result {
            let _ = sqlx::query(
                "INSERT INTO calendar_state (email, calendar_id, last_error) VALUES ($1, $2, $3)
                 ON CONFLICT (email, calendar_id) DO UPDATE SET last_error = $3, updated_at = now()",
            ).bind(email).bind(PRIMARY).bind(error.to_string().chars().take(2000).collect::<String>())
                .execute(&self.p.pool).await;
        }
        result
    }

    async fn sync_locked(&self, email: &str) -> Result<usize, GoogleError> {
        let mut tx = self.p.pool.begin().await.map_err(anyhow::Error::from)?;
        // Serialize cursor ownership across manual calls and overlapping server processes.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("calendar:{email}:{PRIMARY}"))
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
        let token: Option<String> = sqlx::query_scalar(
            "SELECT sync_token FROM calendar_state WHERE email = $1 AND calendar_id = $2",
        )
        .bind(email)
        .bind(PRIMARY)
        .fetch_optional(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?
        .flatten();
        let mut bootstrap = token.is_none();
        let (changed, next) = match self.changes(email, token.as_deref()).await {
            Err(GoogleError::Http { status: 410, .. }) => {
                bootstrap = true;
                self.changes(email, None).await?
            }
            result => result?,
        };
        let now = self.p.clock.now().with_timezone(&Utc);
        let upcoming = self
            .between(email, now, now + chrono::Duration::days(WINDOW_DAYS))
            .await?;
        let mut occurrences = HashMap::new();
        for event in &upcoming {
            if let Some(parent) = event["recurringEventId"].as_str() {
                occurrences.entry(parent.to_string()).or_insert(event);
            }
        }
        sqlx::query(
            "INSERT INTO calendar_state (email, calendar_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        ).bind(email).bind(PRIMARY).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
        if bootstrap {
            sqlx::query("DELETE FROM calendar_events WHERE email = $1 AND calendar_id = $2")
                .bind(email)
                .bind(PRIMARY)
                .execute(&mut *tx)
                .await
                .map_err(anyhow::Error::from)?;
        }
        let old: Vec<(String, Value)> = sqlx::query_as(
            "SELECT event_id, data FROM calendar_events WHERE email = $1 AND calendar_id = $2",
        )
        .bind(email)
        .bind(PRIMARY)
        .fetch_all(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        let mut snapshots: HashMap<String, Value> = old.into_iter().collect();
        let mut count = 0;
        for raw in changed {
            let id = raw["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Calendar event missing id"))?;
            let previous = snapshots.get(id);
            let mut current = project(&raw);
            if current["status"] == "cancelled" {
                // Deleted resources may contain only an id; keep the last known display fields.
                let parent = raw["recurringEventId"]
                    .as_str()
                    .and_then(|id| snapshots.get(id));
                if let Some(previous) = previous.or(parent) {
                    let mut merged = previous.clone();
                    for (key, value) in current.as_object().unwrap() {
                        if !value.is_null() {
                            merged[key] = value.clone();
                        }
                    }
                    if parent.is_some() && raw["originalStartTime"].is_object() {
                        merged.as_object_mut().unwrap().remove("recurrence");
                        merged.as_object_mut().unwrap().remove("occurrence");
                        merged["start"] = raw["originalStartTime"].clone();
                        if let Some(duration) = event_time(&previous["end"])
                            .zip(event_time(&previous["start"]))
                            .map(|(end, start)| end - start)
                        {
                            merged["end"] =
                                if let Some(date) = raw["originalStartTime"]["date"].as_str() {
                                    let date = NaiveDate::parse_from_str(date, "%Y-%m-%d")
                                        .map_err(anyhow::Error::from)?;
                                    json!({"date": (date + duration).to_string()})
                                } else if let Some(start) = event_time(&raw["originalStartTime"]) {
                                    json!({"dateTime": (start + duration).to_rfc3339()})
                                } else {
                                    Value::Null
                                };
                        }
                    }
                    current = merged;
                }
            } else if let Some(occurrence) = occurrences.get(id) {
                current["occurrence"] =
                    json!({"start": occurrence["start"], "end": occurrence["end"]});
            }
            if !bootstrap {
                let cancelled = current["status"] == "cancelled";
                let invitation = previous.is_none()
                    && !cancelled
                    && raw["organizer"]["self"] != true
                    && raw["organizer"]["email"]
                        .as_str()
                        .is_some_and(|e| !e.eq_ignore_ascii_case(email));
                let (kind, topic) = if cancelled {
                    ("event.cancelled", topics::CALENDAR_EVENT_CANCELLED)
                } else if invitation {
                    ("event.invitation", topics::CALENDAR_EVENT_INVITATION)
                } else {
                    ("event.changed", topics::CALENDAR_EVENT_CHANGED)
                };
                let fields: Vec<&str> = [
                    "summary",
                    "start",
                    "end",
                    "location",
                    "organizer",
                    "description_hash",
                    "recurrence",
                    "declined",
                    "status",
                ]
                .into_iter()
                .filter(|key| previous.is_some_and(|p| p[*key] != current[*key]))
                .collect();
                let eligible = !declined(&current)
                    && (within(&current, now) || previous.is_some_and(|p| within(p, now)))
                    && (previous.is_none() || !fields.is_empty())
                    && (!cancelled || previous.is_none_or(|p| p["status"] != "cancelled"));
                let version = raw["updated"].as_str().unwrap_or(&next);
                let data = json!({"calendar_id": PRIMARY, "event": current, "previous": previous, "changes": fields});
                let event_id: Option<i64> = sqlx::query_scalar(
                    "INSERT INTO events (account, source, kind, external_id, status, data)
                     VALUES ($1, 'calendar', $2, $3, $4, $5)
                     ON CONFLICT (source, external_id) DO NOTHING RETURNING id",
                )
                .bind(email)
                .bind(kind)
                .bind(format!("{id}:{version}"))
                .bind(if eligible { "ready" } else { "ignored" })
                .bind(&data)
                .fetch_optional(&mut *tx)
                .await
                .map_err(anyhow::Error::from)?;
                if let Some(event_id) = event_id {
                    count += 1;
                    if eligible {
                        self.p.bus.publish(&mut tx, topic, &json!({"event_id": event_id, "account": email, "calendar_event_id": id}))
                            .await?;
                    }
                }
            }
            snapshots.insert(id.to_string(), current);
        }
        for (id, mut data) in snapshots {
            if data["status"] != "cancelled" && data["recurrence"].is_array() {
                data["occurrence"] = occurrences
                    .get(&id)
                    .map(|e| json!({"start": e["start"], "end": e["end"]}))
                    .unwrap_or(Value::Null);
            }
            // Only metadata needed for future changes is kept, never a description or attendee list.
            sqlx::query(
                "INSERT INTO calendar_events (email, calendar_id, event_id, data) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (email, calendar_id, event_id) DO UPDATE SET data = $4, updated_at = now()
                 WHERE calendar_events.data IS DISTINCT FROM $4 OR $5",
            ).bind(email).bind(PRIMARY).bind(id).bind(&data).bind(within(&data, now)).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
        }
        sqlx::query(
            "UPDATE calendar_state SET sync_token = $3, last_sync_at = now(), last_error = NULL,
             updated_at = now() WHERE email = $1 AND calendar_id = $2",
        )
        .bind(email)
        .bind(PRIMARY)
        .bind(next)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        tx.commit().await.map_err(anyhow::Error::from)?;
        Ok(count)
    }

    pub async fn poll_loop(self: Arc<Self>) {
        loop {
            match self.p.accounts.active_emails().await {
                Ok(emails) => {
                    for email in emails {
                        if let Err(error) = self.sync(&email).await {
                            tracing::warn!(account = email, %error, "Calendar sync failed");
                        }
                    }
                }
                Err(error) => tracing::error!(%error, "Calendar accounts query failed"),
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    }

    pub async fn notify(&self, payload: Value, _ctx: JobContext) -> anyhow::Result<Outcome> {
        #[derive(serde::Deserialize)]
        struct EventRef {
            event_id: i64,
        }
        let r: EventRef = serde_json::from_value(payload)?;
        let mut tx = self.p.pool.begin().await?;
        let row: Option<(String, String, String, Value)> = sqlx::query_as(
            "SELECT account, kind, status, data FROM events WHERE id = $1 AND source = 'calendar' FOR UPDATE",
        ).bind(r.event_id).fetch_optional(&mut *tx).await?;
        let Some((account, kind, status, data)) = row else {
            return Ok(Outcome::Done);
        };
        if status != "ready" {
            return Ok(Outcome::Done);
        }
        let owner = self.p.accounts.owner_of(&account).await?;
        let tr = self.p.user_settings.tr(&owner).await;
        let now = self.p.clock.now();
        let rendered = notification(&tr, &kind, &data);
        self.p
            .notifier
            .send(&OutgoingMessage {
                text: rendered.text,
                entities: rendered.entities,
                silent: !self.p.model.hours().is_on(now),
                ..Default::default()
            })
            .await?;
        sqlx::query("UPDATE events SET status = 'notified', updated_at = now() WHERE id = $1")
            .bind(r.event_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Outcome::Done)
    }

    pub async fn agenda(
        &self,
        owner: &str,
        date: NaiveDate,
        tr: &Tr,
    ) -> anyhow::Result<Vec<String>> {
        let start = midnight(date)?;
        let end = midnight(
            date.succ_opt()
                .ok_or_else(|| anyhow::anyhow!("Calendar date overflow"))?,
        )?;
        let mut events = Vec::new();
        for email in self.p.accounts.active_emails().await? {
            if self.p.accounts.owner_of(&email).await? == owner && self.connected(&email).await? {
                events.extend(self.between(&email, start, end).await?);
            }
        }
        events.sort_by_key(|e| event_time(&e["start"]));
        Ok(events
            .iter()
            .map(|e| format!("• {} · {}", time_range(tr, e), title(tr, e)))
            .collect())
    }
}

fn project(raw: &Value) -> Value {
    let mut data = json!({});
    for key in [
        "id",
        "updated",
        "status",
        "summary",
        "start",
        "end",
        "location",
        "organizer",
        "recurrence",
        "recurringEventId",
        "htmlLink",
    ] {
        data[key] = raw[key].clone();
    }
    if raw["status"] != "cancelled" {
        data["description_hash"] = json!(format!(
            "{:x}",
            Sha256::digest(raw["description"].as_str().unwrap_or_default())
        ));
        data["declined"] = json!(declined(raw));
    }
    data
}

fn declined(e: &Value) -> bool {
    e["declined"] == true
        || e["attendees"].as_array().is_some_and(|a| {
            a.iter()
                .any(|a| a["self"] == true && a["responseStatus"] == "declined")
        })
}

fn midnight(date: NaiveDate) -> anyhow::Result<DateTime<Utc>> {
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .map(|d| d.with_timezone(&Utc))
        .ok_or_else(|| anyhow::anyhow!("Calendar date has no local midnight"))
}

fn event_time(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = value["dateTime"].as_str() {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    } else {
        midnight(NaiveDate::parse_from_str(value["date"].as_str()?, "%Y-%m-%d").ok()?).ok()
    }
}

fn display(e: &Value) -> &Value {
    if e["occurrence"].is_object() {
        &e["occurrence"]
    } else {
        e
    }
}

fn within(e: &Value, now: DateTime<Utc>) -> bool {
    if e["recurrence"].is_array() && !e["occurrence"].is_object() {
        return false;
    }
    let times = display(e);
    event_time(&times["start"])
        .zip(event_time(&times["end"]))
        .is_some_and(|(start, end)| end > now && start < now + chrono::Duration::days(WINDOW_DAYS))
}

fn title(tr: &Tr, e: &Value) -> String {
    e["summary"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(500)
                .collect()
        })
        .unwrap_or_else(|| tr.get("calendar-untitled"))
}

fn time_range(tr: &Tr, e: &Value) -> String {
    if e["recurrence"].is_array() && !e["occurrence"].is_object() {
        return tr.get("calendar-no-upcoming");
    }
    let e = display(e);
    if let Some(date) = e["start"]["date"].as_str() {
        let end = e["end"]["date"]
            .as_str()
            .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
            .and_then(|d| d.pred_opt())
            .map(|d| d.to_string());
        return format!(
            "{} · {date}{}",
            tr.get("calendar-all-day"),
            end.filter(|d| d != date)
                .map(|d| format!(" / {d}"))
                .unwrap_or_default()
        );
    }
    let start = event_time(&e["start"]).map(|d| d.with_timezone(&Local));
    let end = event_time(&e["end"]).map(|d| d.with_timezone(&Local));
    match (start, end) {
        (Some(start), Some(end)) if start.date_naive() == end.date_naive() => format!(
            "{} · {}–{} (UTC{})",
            start.format("%Y-%m-%d"),
            start.format("%H:%M"),
            end.format("%H:%M"),
            start.offset()
        ),
        (Some(start), Some(end)) => format!(
            "{} – {} (UTC{})",
            start.format("%Y-%m-%d %H:%M"),
            end.format("%Y-%m-%d %H:%M"),
            end.offset()
        ),
        _ => tr.get("calendar-time-unknown"),
    }
}
