//! `gm status`: what is actually installed and running on this Mac, read from the live system
//! (launchd, the processes, the endpoints, the database), never from the repo. Needs no secrets.
//!
//! Checks: the launchd agents `com.gm.server` and `com.gm.backup` (loaded, running, last exit code),
//! whether the running server is the latest build of its binary, `/healthz`, the model's readiness
//! probe and model name, migrations (pending, unknown, or edited after being applied), and the queue
//! (dead jobs, open health incidents, last Gmail sync). Exit code 1 when any check fails.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

use crate::config::Config;
use crate::health;

pub const AGENTS: [&str; 2] = ["com.gm.server", "com.gm.backup"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: String,
    pub state: State,
    pub detail: String,
}

impl Check {
    fn new(name: impl Into<String>, state: State, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state,
            detail: detail.into(),
        }
    }
}

/// The top-level fields of `launchctl print gui/<uid>/<label>`.
#[derive(Debug, Default, PartialEq)]
pub struct Agent {
    pub state: String,
    pub pid: Option<u32>,
    pub last_exit: Option<i32>,
    pub program: String,
}

pub fn parse_launchctl(text: &str) -> Agent {
    let mut a = Agent::default();
    for line in text.lines() {
        // Top-level fields are indented by exactly one tab; nested blocks by more.
        let Some(rest) = line.strip_prefix('\t') else {
            continue;
        };
        if rest.starts_with('\t') {
            continue;
        }
        let Some((k, v)) = rest.split_once(" = ") else {
            continue;
        };
        match k {
            "state" => a.state = v.trim().to_string(),
            "pid" => a.pid = v.trim().parse().ok(),
            "last exit code" => a.last_exit = v.trim().parse().ok(),
            "program" => a.program = v.trim().to_string(),
            _ => {}
        }
    }
    a
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn uid() -> String {
    run("id", &["-u"]).unwrap_or_default().trim().to_string()
}

/// One check per agent, plus whether the server runs the latest build of its binary.
pub fn agents() -> Vec<Check> {
    let uid = uid();
    let mut out = vec![];
    for label in AGENTS {
        let Some(text) = run("launchctl", &["print", &format!("gui/{uid}/{label}")]) else {
            out.push(Check::new(
                label,
                State::Fail,
                "not loaded (see deploy/README.md)",
            ));
            continue;
        };
        let a = parse_launchctl(&text);
        let exit = a.last_exit.unwrap_or(0);
        let check = match (label, a.pid) {
            ("com.gm.server", Some(pid)) => {
                Check::new(label, State::Ok, format!("running, pid {pid}"))
            }
            ("com.gm.server", None) => Check::new(
                label,
                State::Fail,
                format!("loaded but not running (last exit {exit})"),
            ),
            (_, _) if exit != 0 => Check::new(
                label,
                State::Fail,
                format!("last run exited with {exit} (log: ~/Library/Logs/gm/)"),
            ),
            (_, _) => Check::new(label, State::Ok, format!("loaded, last exit {exit}")),
        };
        out.push(check);
        if label == "com.gm.server"
            && let Some(pid) = a.pid
        {
            out.push(build_check(pid, &a.program));
        }
    }
    out
}

/// Whether the process started after its binary was last built (otherwise a restart is due).
fn build_check(pid: u32, program: &str) -> Check {
    // `etime` ([[dd-]hh:]mm:ss) doesn't depend on the locale, unlike `lstart`.
    let started = run("ps", &["-o", "etime=", "-p", &pid.to_string()])
        .and_then(|s| parse_etime(&s))
        .map(|secs| chrono::Local::now() - chrono::Duration::seconds(secs));
    let built = std::fs::metadata(program)
        .and_then(|m| m.modified())
        .ok()
        .map(DateTime::<chrono::Local>::from);
    match (started, built) {
        (Some(s), Some(b)) if rebuilt_after_start(b, s) => Check::new(
            "running build",
            State::Warn,
            format!(
                "{program} was rebuilt at {} after the server started at {}: restart to run it",
                b.format("%m-%d %H:%M"),
                s.format("%m-%d %H:%M")
            ),
        ),
        (Some(s), Some(_)) => Check::new(
            "running build",
            State::Ok,
            format!("{program}, started {}", s.format("%m-%d %H:%M")),
        ),
        _ => Check::new(
            "running build",
            State::Warn,
            "could not compare start and build times",
        ),
    }
}

/// Whether the binary changed after the process started. `ps` gives the start in whole seconds and
/// the usual upgrade (`cargo build && launchctl kickstart`) starts the server within a second of the
/// build, so a newer binary only counts when it is more than 2 s newer.
pub fn rebuilt_after_start<Tz: chrono::TimeZone>(
    built: DateTime<Tz>,
    started: DateTime<Tz>,
) -> bool {
    built > started + chrono::Duration::seconds(2)
}

/// Seconds from `ps -o etime` output: `[[dd-]hh:]mm:ss`.
pub fn parse_etime(s: &str) -> Option<i64> {
    let s = s.trim();
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<i64>().ok()?, r),
        None => (0, s),
    };
    let parts: Vec<i64> = rest
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let (h, m, sec) = match parts.as_slice() {
        [m, s] => (0, *m, *s),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    Some(((days * 24 + h) * 60 + m) * 60 + sec)
}

async fn get_json(http: &reqwest::Client, url: &str) -> Result<(u16, Value), String> {
    let resp = http
        .get(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let code = resp.status().as_u16();
    Ok((code, resp.json().await.unwrap_or(Value::Null)))
}

/// `/healthz` of the configured web server and the model's readiness probe.
pub async fn endpoints(cfg: &Config, model: &crate::model::ModelSettings) -> Vec<Check> {
    let http = reqwest::Client::new();
    let web = match get_json(
        &http,
        &format!("{}/healthz", cfg.base_url.trim_end_matches('/')),
    )
    .await
    {
        Ok((200, _)) => Check::new(
            "web",
            State::Ok,
            format!("{} answers /healthz", cfg.base_url),
        ),
        Ok((code, _)) => Check::new("web", State::Fail, format!("/healthz returned {code}")),
        Err(e) => Check::new(
            "web",
            State::Fail,
            format!("{} unreachable: {e}", cfg.base_url),
        ),
    };
    let model = match get_json(&http, &cfg.llm_ready_url).await {
        Ok((code, body)) if (200..300).contains(&code) => {
            let served = body["model"].as_str().unwrap_or("?");
            if served == model.model || served == "?" {
                Check::new("model", State::Ok, format!("ready: {served}"))
            } else {
                Check::new(
                    "model",
                    State::Warn,
                    format!(
                        "ready, but serves {served} while the configured model is {}",
                        model.model
                    ),
                )
            }
        }
        // Stopped on purpose: gm stops the model outside model hours and starts it on demand.
        _ if model.manage && !model.hours().is_on(chrono::Local::now()) => Check::new(
            "model",
            State::Ok,
            "stopped outside model hours (gm starts it for your requests)",
        ),
        Ok((code, _)) => Check::new(
            "model",
            State::Fail,
            format!("{} returned {code}", cfg.llm_ready_url),
        ),
        Err(e) => Check::new(
            "model",
            State::Fail,
            format!("{} unreachable: {e}", cfg.llm_ready_url),
        ),
    };
    vec![web, model]
}

/// Migrations in this binary versus the database: pending, unknown, or edited after being applied
/// (sqlx refuses to start then).
pub async fn migrations(pool: &PgPool) -> anyhow::Result<Check> {
    // An empty database has no `_sqlx_migrations`: everything is pending.
    let applied: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT version, checksum FROM _sqlx_migrations WHERE success ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut pending = vec![];
    let mut edited = vec![];
    for m in crate::db::MIGRATOR.iter() {
        match applied.iter().find(|(v, _)| *v == m.version) {
            None => pending.push(m.version),
            Some((_, sum)) if sum.as_slice() != m.checksum.as_ref() => edited.push(m.version),
            Some(_) => {}
        }
    }
    let unknown: Vec<i64> = applied
        .iter()
        .map(|(v, _)| *v)
        .filter(|v| !crate::db::MIGRATOR.iter().any(|m| m.version == *v))
        .collect();
    let latest = applied.last().map(|(v, _)| *v).unwrap_or(0);
    Ok(if !edited.is_empty() {
        Check::new(
            "migrations",
            State::Fail,
            format!("applied migrations {edited:?} were edited since; gm will refuse to start"),
        )
    } else if !unknown.is_empty() {
        Check::new(
            "migrations",
            State::Fail,
            format!("database has migrations {unknown:?} this build doesn't know (older build?)"),
        )
    } else if !pending.is_empty() {
        Check::new(
            "migrations",
            State::Warn,
            format!("applied up to {latest}; {pending:?} apply at the next start of this build"),
        )
    } else {
        Check::new("migrations", State::Ok, format!("up to date ({latest})"))
    })
}

/// Dead jobs, open health incidents and the last Gmail sync, with the health checker's queries and
/// lag limit (in Pub/Sub mode, assuming push delivery works). `pubsub_safety_poll_s` is `Some` in
/// Pub/Sub mode.
pub async fn queue(pool: &PgPool, pubsub_safety_poll_s: Option<u64>) -> anyhow::Result<Vec<Check>> {
    let dead = health::dead_jobs(pool).await?;
    let synced = health::oldest_sync(pool).await?;
    let limit = health::lag_limit(pubsub_safety_poll_s, true);
    let has_health: bool = sqlx::query_scalar("SELECT to_regclass('health_state') IS NOT NULL")
        .fetch_one(pool)
        .await?;
    let incidents: Vec<String> = if has_health {
        sqlx::query_scalar("SELECT detail FROM health_state ORDER BY failing_since")
            .fetch_all(pool)
            .await?
    } else {
        vec![]
    };
    let mut out = vec![if dead > 0 {
        Check::new("dead jobs", State::Fail, format!("{dead} (see /jobs)"))
    } else {
        Check::new("dead jobs", State::Ok, "none")
    }];
    out.push(match synced {
        Some(t) => {
            let mins = (Utc::now() - t).num_minutes();
            Check::new(
                "gmail sync",
                if Utc::now() - t > limit {
                    State::Warn
                } else {
                    State::Ok
                },
                format!("oldest mailbox synced {mins} min ago"),
            )
        }
        None => Check::new("gmail sync", State::Warn, "no active mailbox"),
    });
    if !incidents.is_empty() {
        out.push(Check::new("health", State::Fail, incidents.join("; ")));
    }
    Ok(out)
}

/// Every check, in display order.
pub async fn collect(cfg: &Config) -> Vec<Check> {
    let mut out = agents();
    let pool = crate::db::connect(&cfg.database_url).await;
    let model = match &pool {
        Ok(p) => crate::model::ModelSettings::load(p, cfg)
            .await
            .unwrap_or_else(|_| crate::model::ModelSettings::from_config(cfg)),
        Err(_) => crate::model::ModelSettings::from_config(cfg),
    };
    out.extend(endpoints(cfg, &model).await);
    match pool {
        Ok(pool) => {
            match migrations(&pool).await {
                Ok(c) => out.push(c),
                Err(e) => out.push(Check::new("migrations", State::Fail, e.to_string())),
            }
            match queue(&pool, cfg.pubsub.as_ref().map(|p| p.safety_poll_interval_s)).await {
                Ok(cs) => out.extend(cs),
                Err(e) => out.push(Check::new("queue", State::Warn, e.to_string())),
            }
        }
        Err(e) => out.push(Check::new(
            "database",
            State::Fail,
            format!("unreachable: {e}"),
        )),
    }
    out
}

pub fn format(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|c| {
            let mark = match c.state {
                State::Ok => "✓",
                State::Warn => "!",
                State::Fail => "✗",
            };
            format!("{mark} {:<14} {}\n", c.name, c.detail)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_in_the_same_second_as_the_start_is_not_stale() {
        use chrono::TimeZone;
        // Seen on the Mac: built at :07.875, `ps etime` put the start at :07 (whole seconds).
        let at = |s: i64, ns: u32| chrono::Utc.timestamp_opt(s, ns).unwrap();
        assert!(!rebuilt_after_start(
            at(1_794_919_507, 875_113_114),
            at(1_794_919_507, 0)
        ));
        assert!(rebuilt_after_start(
            at(1_794_919_600, 0),
            at(1_794_919_507, 0)
        ));
    }

    const RUNNING: &str = "gui/501/com.gm.server = {\n\tactive count = 1\n\tpath = /x.plist\n\tstate = running\n\n\tprogram = /repo/target/release/gm\n\targuments = {\n\t\tstate = active\n\t}\n\tpid = 64331\n\tlast exit code = 0\n}\n";
    const EXITED: &str = "gui/501/com.gm.backup = {\n\tstate = not running\n\tprogram = /bin/bash\n\tlast exit code = 126\n\tjob state = exited\n}\n";

    #[test]
    fn parses_top_level_launchctl_fields_only() {
        assert_eq!(
            parse_launchctl(RUNNING),
            Agent {
                state: "running".into(),
                pid: Some(64331),
                last_exit: Some(0),
                program: "/repo/target/release/gm".into(),
            }
        );
        let b = parse_launchctl(EXITED);
        assert_eq!(
            (b.pid, b.last_exit, b.state.as_str()),
            (None, Some(126), "not running")
        );
    }

    #[test]
    fn parses_elapsed_time() {
        assert_eq!(parse_etime("  05:07\n"), Some(307));
        assert_eq!(parse_etime("10:57:12"), Some(39432));
        assert_eq!(parse_etime("2-01:00:00"), Some(176400));
        assert_eq!(parse_etime("junk"), None);
    }

    #[test]
    fn formats_marks() {
        let s = format(&[
            Check::new("a", State::Ok, "fine"),
            Check::new("b", State::Fail, "broken"),
        ]);
        assert_eq!(s, "✓ a              fine\n✗ b              broken\n");
    }
}
