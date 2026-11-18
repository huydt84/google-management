//! Eval set and `gm eval` (2F): dashboard "Add to eval", the run report, local-only.
mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use common::*;
use gm::eval::{Added, EvalCase};
use gm::rules::{Action, RuleSet};
use serde_json::Value;
use sqlx::PgPool;

fn temp_eval_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("gm-eval-test-{tag}-{}-{nanos}", std::process::id()))
}

async fn event(t: &TestApp, message_id: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO events (account, source, kind, external_id, status, rule, data)
         VALUES ($1, 'gmail', 'message.received', $2, 'notified', 'model', '{\"subject\": \"S\"}') RETURNING id",
    )
    .bind(OWNER)
    .bind(message_id)
    .fetch_one(&t.app.pool)
    .await
    .unwrap()
}

fn mode(p: &std::path::Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn dashboard_adds_updates_and_limits_the_eval_set(pool: PgPool) {
    let dir = temp_eval_dir("web");
    let d = dir.clone();
    let t = test_app(pool, move |c| c.eval_dir = d, None).await;
    t.connect_owner().await;
    t.mock_message(gmail_message(
        "e1",
        "Shop <info@shop.example.net>",
        "Your receipt",
        &["INBOX"],
        "Thanks.",
    ))
    .await;
    let id = event(&t, "e1").await;
    let router = gm::web::router(t.app.state.clone());
    let cookie = session_cookie(&t, OWNER);
    let url = format!("/events/{id}/eval");

    let (status, headers, _) = send(&router, post_form(&url, &cookie, "expected=digest")).await;
    assert_eq!(status, 303);
    assert_eq!(headers["location"], format!("/?eval=added#event-{id}"));
    let file = dir.join("e1.json");
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&file), 0o600);
    let case: EvalCase = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(
        (
            case.expected,
            case.account.as_str(),
            case.message["id"].as_str()
        ),
        (Action::Digest, OWNER, Some("e1"))
    );

    // Adding again updates the expected action.
    let (_, headers, _) = send(
        &router,
        post_form(&format!("{url}?before=2"), &cookie, "expected=ignore"),
    )
    .await;
    assert_eq!(
        headers["location"],
        format!("/?eval=updated&before=2#event-{id}")
    );
    assert_eq!(gm::eval::load(&dir).unwrap()[0].expected, Action::Ignore);
    let location = headers["location"]
        .to_str()
        .unwrap()
        .split('#')
        .next()
        .unwrap();
    let (_, _, body) = send(&router, get(location, Some(&cookie))).await;
    assert!(body.contains("Eval case updated."));
    assert!(body.contains("In the eval set"));

    // Bad input, missing event, and no session.
    let (status, _, _) = send(&router, post_form(&url, &cookie, "expected=triage")).await;
    assert_eq!(status, 400);
    let (_, headers, _) = send(
        &router,
        post_form("/events/999999/eval", &cookie, "expected=notify"),
    )
    .await;
    assert_eq!(headers["location"], "/?eval=gone#event-999999");
    let (status, _, _) = send(
        &router,
        post_form(&url, "gm_session=bogus", "expected=notify"),
    )
    .await;
    assert_eq!(status, 401);

    // At most 200 cases: a new one is refused once the folder is full.
    for i in 1..gm::eval::MAX_CASES {
        std::fs::write(dir.join(format!("filler{i:03}.json")), "{}").unwrap();
    }
    t.mock_message(gmail_message("e2", "A <a@b.example>", "S", &["INBOX"], "x"))
        .await;
    let id2 = event(&t, "e2").await;
    let (_, headers, _) = send(
        &router,
        post_form(&format!("/events/{id2}/eval"), &cookie, "expected=notify"),
    )
    .await;
    assert_eq!(headers["location"], format!("/?eval=full#event-{id2}"));
    assert!(!dir.join("e2.json").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

fn case(id: &str, from: &str, subject: &str, expected: Action) -> EvalCase {
    EvalCase {
        message_id: id.into(),
        account: OWNER.into(),
        added_at: chrono::Utc::now(),
        expected,
        notes: String::new(),
        message: gmail_message(id, from, subject, &["INBOX"], "body"),
    }
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn run_reports_accuracy_confusion_parse_rate_and_calls(pool: PgPool) {
    let t = test_app(pool, |_| {}, None).await;
    t.mock_llm_for(TRIAGE, &triage_json("ignore", "a receipt"))
        .await;
    t.mock_llm_for(SUMMARY, SUMMARY_JSON).await;
    t.mock_llm_for("Task: list the job postings", "not json at all")
        .await;
    let dir = temp_eval_dir("run");
    let cases = [
        // Rule: notify without summary.
        case(
            "r1",
            "Rakuten <no-reply@id.rakuten.co.jp>",
            "[楽天] ログイン通知",
            Action::Notify,
        ),
        // Rule: notify with summary.
        case(
            "r2",
            "LinkedIn <messages-noreply@linkedin.com>",
            "New message",
            Action::Notify,
        ),
        // Rule: jobs digest; the model output never parses.
        case(
            "r3",
            "LinkedIn Job Alerts <jobalerts-noreply@linkedin.com>",
            "new jobs",
            Action::Digest,
        ),
        // No rule: the model says ignore, the owner wanted digest.
        case(
            "m1",
            "Shop <info@shop.example.net>",
            "Your receipt",
            Action::Digest,
        ),
    ];
    for c in &cases {
        assert_eq!(gm::eval::add(&dir, c).unwrap(), Added::New);
    }
    let cases = gm::eval::load(&dir).unwrap();
    assert_eq!(cases.len(), 4);

    let mut cfg = t.app.cfg.clone();
    cfg.llm_base_url = format!("{}/v1", t.server.uri());
    let mut llm = gm::llm::LlmClient::new(&cfg, None).unwrap();
    let calls = llm.record_calls();
    let rules = RuleSet::parse(&std::fs::read_to_string(&cfg.rules_path).unwrap()).unwrap();
    let r = gm::eval::run(&llm, &calls, &rules, &cases, "English")
        .await
        .unwrap();

    assert_eq!(r.confusion[&("notify", "notify")], 2);
    assert_eq!(r.confusion[&("digest", "digest")], 1);
    assert_eq!(r.confusion[&("digest", "ignore")], 1);
    assert_eq!(r.wrong, vec![("m1".to_string(), "digest", "ignore")]);
    assert_eq!(r.parse["triage"], (1, 1));
    assert_eq!(r.parse["summary"], (1, 1));
    assert_eq!(r.parse["digest"], (0, 1));
    let tasks: Vec<&str> = r.calls.iter().map(|c| c.task.as_str()).collect();
    // Cases run in file-name order: m1, r1, r2, r3.
    assert_eq!(
        tasks,
        ["triage", "summary", "digest-jobs", "digest-jobs-retry"]
    );

    let text = gm::eval::format_report(&r, "test-model");
    assert!(
        text.contains("gm eval: 4 cases, model test-model"),
        "{text}"
    );
    assert!(text.contains("overall  3/4 (75%)"), "{text}");
    assert!(text.contains("m1: expected digest, got ignore"), "{text}");
    assert!(text.contains("digest-jobs-retry"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn eval_files_with_bad_content_are_skipped() {
    let dir = temp_eval_dir("bad");
    gm::eval::add(&dir, &case("ok", "A <a@b.example>", "S", Action::Ignore)).unwrap();
    std::fs::write(dir.join("broken.json"), "{").unwrap();
    let mut triage: Value =
        serde_json::to_value(case("t", "A <a@b.example>", "S", Action::Ignore)).unwrap();
    triage["expected"] = "triage".into();
    std::fs::write(dir.join("t.json"), triage.to_string()).unwrap();
    let cases = gm::eval::load(&dir).unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].message_id, "ok");
    assert!(gm::eval::ensure_local("http://10.0.0.5:8000/v1").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
