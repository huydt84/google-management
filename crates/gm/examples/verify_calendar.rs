//! Manual live check. Never run from automated tests; requires the owner's Google consent.

use std::sync::Arc;

use anyhow::ensure;
use chrono::{Duration, Local, Utc};
use gm::app::{Overrides, build};
use gm::config::Config;
use gm::google::CALENDAR_READONLY;
use gm::notify::RecordingNotifier;
use gm::web::session::{SESSION_COOKIE, Session};

#[derive(clap::Parser)]
struct Args {
    /// Restrict the change-state report to one Google Calendar event id.
    #[arg(long)]
    event_id: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = <Args as clap::Parser>::parse();
    let cfg = Config::from_env()?;
    let pool = gm::db::connect(&cfg.database_url).await?;
    let app = build(
        cfg.clone(),
        pool.clone(),
        Overrides {
            notifier: Some(Arc::new(RecordingNotifier::default())),
            ..Overrides::default()
        },
    )?;
    let account = app
        .state
        .accounts
        .get(&cfg.allowed_email)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Connect Google through the dashboard first"))?;
    ensure!(account.status == "active", "Google account is not active");
    ensure!(
        account.scopes.0.iter().any(|s| s == CALENDAR_READONLY),
        "Reconnect Google to grant calendar.readonly"
    );
    println!("Google account active; calendar.readonly granted");

    let bootstrapped: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM calendar_state WHERE email = $1 AND sync_token IS NOT NULL)",
    )
    .bind(&cfg.allowed_email)
    .fetch_one(&pool)
    .await?;
    let inserted = app.calendar.sync(&cfg.allowed_email).await?;
    let (token, synced, error): (bool, Option<chrono::DateTime<Utc>>, Option<String>) =
        sqlx::query_as(
            "SELECT sync_token IS NOT NULL, last_sync_at, last_error FROM calendar_state
         WHERE email = $1 AND calendar_id = 'primary'",
        )
        .bind(&cfg.allowed_email)
        .fetch_one(&pool)
        .await?;
    ensure!(
        token && synced.is_some() && error.is_none(),
        "Calendar cursor is not healthy"
    );
    if !bootstrapped {
        ensure!(inserted == 0, "Bootstrap must not replay existing events");
    }
    println!(
        "Real Calendar {} succeeded; {inserted} new change rows; cursor present; last_error clear",
        if bootstrapped {
            "incremental sync"
        } else {
            "bootstrap"
        }
    );
    let snapshots: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM calendar_events WHERE email = $1 AND calendar_id = 'primary'",
    )
    .bind(&cfg.allowed_email)
    .fetch_one(&pool)
    .await?;
    println!("Stored minimal Calendar snapshots: {snapshots}");

    let prefs = app.pipeline.user_settings.get(&cfg.allowed_email).await?;
    let tr = gm::i18n::Tr::new(&prefs.ui_language);
    let today = Local::now().date_naive();
    let agenda = app.calendar.agenda(&cfg.allowed_email, today, &tr).await?;
    println!(
        "Live digest agenda for {today}: {} eligible events",
        agenda.len()
    );
    let now = Utc::now();
    let upcoming = app
        .calendar
        .api
        .events(
            &cfg.allowed_email,
            "primary",
            &[
                ("singleEvents", "true".into()),
                ("orderBy", "startTime".into()),
                ("timeMin", now.to_rfc3339()),
                ("timeMax", (now + Duration::days(30)).to_rfc3339()),
            ],
        )
        .await?;
    println!(
        "Google's live next-30-day query returned {} expanded events before eligibility filtering",
        upcoming.items.len()
    );

    if let Some(event_id) = &args.event_id {
        let tracked = upcoming
            .items
            .iter()
            .find(|event| event["id"].as_str() == Some(event_id));
        println!(
            "Tracked event present in Google's upcoming results: {}",
            tracked.is_some()
        );
        if let Some(event) = tracked {
            let date = event["start"]["dateTime"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|time| time.with_timezone(&Local).date_naive())
                .or_else(|| event["start"]["date"].as_str().and_then(|s| s.parse().ok()));
            if let Some(date) = date {
                let lines = app.calendar.agenda(&cfg.allowed_email, date, &tr).await?;
                let included = event["summary"]
                    .as_str()
                    .is_some_and(|title| lines.iter().any(|line| line.contains(title)));
                println!("Tracked event included in live agenda for {date}: {included}");
            }
        }
    }

    let session = Session {
        email: cfg.allowed_email.clone(),
        exp: now.timestamp() + 120,
        iat: now.timestamp(),
    };
    let response = app
        .state
        .http
        .get(&cfg.base_url)
        .header(
            reqwest::header::COOKIE,
            format!(
                "{SESSION_COOKIE}={}",
                app.state.signer.sign("session", &session)
            ),
        )
        .send()
        .await?;
    ensure!(
        response.status().is_success(),
        "Dashboard returned {}",
        response.status()
    );
    let dashboard = response.text().await?;
    ensure!(
        dashboard.contains(&cfg.allowed_email),
        "Dashboard did not accept the owner's session"
    );
    ensure!(
        !dashboard.contains(&tr.get("missing-scopes")),
        "Dashboard still reports missing Google scopes"
    );
    println!("Live authenticated dashboard: HTTP 200; no missing-scope notice");

    let changes: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT kind, status, count(*) FROM events WHERE source = 'calendar' AND account = $1
         AND ($2::text IS NULL OR split_part(external_id, ':', 1) = $2)
         GROUP BY kind, status ORDER BY kind, status",
    )
    .bind(&cfg.allowed_email)
    .bind(&args.event_id)
    .fetch_all(&pool)
    .await?;
    for (kind, status, count) in changes {
        println!("Recorded Calendar changes: {kind}, {status}, {count}");
    }
    println!(
        "No Calendar writes or synthetic Telegram messages were sent by this verification command"
    );
    Ok(())
}
