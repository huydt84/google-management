mod common;

use chrono::{Duration, Utc};
use common::*;
use gm::google::CALENDAR_READONLY;
use serde_json::{Value, json};
use sqlx::PgPool;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockGuard, ResponseTemplate};

const EVENTS: &str = "/calendar/v3/calendars/primary/events";

async fn connect(t: &TestApp) {
    t.connect_owner().await;
    t.app
        .state
        .accounts
        .save(OWNER, OWNER, "refresh-token", &[CALENDAR_READONLY.into()])
        .await
        .unwrap();
}

fn event(id: &str, hour: u32) -> Value {
    json!({"id": id, "updated": "2026-11-17T01:00:00Z", "status": "confirmed",
        "summary": "Planning", "description": "untrusted private description",
        "organizer": {"email": "organizer@example.com", "displayName": "Organizer"},
        "start": {"dateTime": local(hour, 0).to_rfc3339()},
        "end": {"dateTime": local(hour, 0).checked_add_signed(Duration::hours(1)).unwrap().to_rfc3339()}})
}

async fn sync_page(t: &TestApp, items: Vec<Value>, token: &str) -> MockGuard {
    Mock::given(method("GET"))
        .and(path(EVENTS))
        .and(query_param("singleEvents", "false"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"items": items, "nextSyncToken": token})),
        )
        .with_priority(10)
        .mount_as_scoped(&t.server)
        .await
}

async fn upcoming(t: &TestApp, items: Vec<Value>) -> MockGuard {
    Mock::given(method("GET"))
        .and(path(EVENTS))
        .and(query_param("singleEvents", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": items})))
        .with_priority(10)
        .mount_as_scoped(&t.server)
        .await
}

async fn cursor(t: &TestApp) -> String {
    sqlx::query_scalar("SELECT sync_token FROM calendar_state WHERE email = $1")
        .bind(OWNER)
        .fetch_one(&t.app.pool)
        .await
        .unwrap()
}

async fn event_count(t: &TestApp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(&t.app.pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn bootstrap_change_invitation_retry_and_sparse_cancellation(pool: PgPool) {
    let t = test_app(
        pool,
        |cfg| {
            cfg.model_on = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
            cfg.model_off = chrono::NaiveTime::from_hms_opt(19, 0, 0).unwrap();
        },
        Some(local(20, 0)),
    )
    .await;
    connect(&t).await;
    let mut old = event("existing", 22);
    let _agenda = upcoming(&t, vec![]).await;
    let first = sync_page(&t, vec![old.clone()], "s1").await;
    assert_eq!(t.app.calendar.sync(OWNER).await.unwrap(), 0);
    assert_eq!(event_count(&t).await, 0);
    assert_eq!(cursor(&t).await, "s1");
    drop(first);

    old["summary"] = json!("Planning revised");
    old["updated"] = json!("2026-11-17T02:00:00Z");
    old["start"]["dateTime"] = json!(local(23, 0).to_rfc3339());
    old["end"]["dateTime"] = json!((local(23, 0) + Duration::hours(1)).to_rfc3339());
    let second = sync_page(&t, vec![old.clone(), event("invitation", 21)], "s2").await;
    assert_eq!(t.app.calendar.sync(OWNER).await.unwrap(), 2);
    t.drain().await;
    assert_eq!(t.sent().len(), 2);
    assert!(t.sent().iter().all(|m| m.silent && m.buttons.is_empty()));
    let text = t
        .sent()
        .iter()
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Event changed\nPlanning revised"), "{text}");
    assert!(text.contains("22:00") && text.contains("23:00"), "{text}");
    assert!(text.contains("New invitation\nPlanning"));
    assert!(!text.contains("untrusted private description"));
    assert_eq!(t.app.calendar.sync(OWNER).await.unwrap(), 0);
    t.drain().await;
    assert_eq!(t.sent().len(), 2, "a replay sends nothing twice");
    drop(second);

    let _cancel = sync_page(
        &t,
        vec![json!({"id": "existing", "status": "cancelled"})],
        "s3",
    )
    .await;
    assert_eq!(t.app.calendar.sync(OWNER).await.unwrap(), 1);
    t.drain().await;
    assert_eq!(t.sent().len(), 3);
    assert!(
        t.sent()[2]
            .text
            .contains("Event cancelled\nPlanning revised")
    );
    assert!(t.sent()[2].text.contains("23:00"));
    let stored: String =
        sqlx::query_scalar("SELECT data::text FROM calendar_events WHERE event_id = 'existing'")
            .fetch_one(&t.app.pool)
            .await
            .unwrap();
    assert!(!stored.contains("untrusted private description"));
    let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_calls")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(calls, 0);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn declined_past_distant_and_metadata_only_changes_do_not_ping(pool: PgPool) {
    let t = test_app(pool, |_| {}, Some(local(12, 0))).await;
    connect(&t).await;
    let _agenda = upcoming(&t, vec![]).await;
    let first = sync_page(&t, vec![event("metadata", 14)], "s1").await;
    t.app.calendar.sync(OWNER).await.unwrap();
    drop(first);
    let mut declined = event("declined", 14);
    declined["attendees"] = json!([{"self": true, "responseStatus": "declined"}]);
    let mut distant = event("distant", 14);
    for key in ["start", "end"] {
        distant[key]["dateTime"] = json!((local(14, 0) + Duration::days(31)).to_rfc3339());
    }
    let mut metadata = event("metadata", 14);
    metadata["updated"] = json!("2026-11-17T02:00:00Z");
    let mut all_day = event("all-day", 0);
    all_day["organizer"] = json!({"self": true, "email": OWNER});
    all_day["start"] = json!({"date": test_day().to_string()});
    all_day["end"] = json!({"date": (test_day() + Duration::days(2)).to_string()});
    let _changes = sync_page(
        &t,
        vec![declined, distant, event("past", 8), metadata, all_day],
        "s2",
    )
    .await;
    assert_eq!(t.app.calendar.sync(OWNER).await.unwrap(), 5);
    t.drain().await;
    assert_eq!(t.sent().len(), 1);
    assert!(t.sent()[0].text.contains("All day"));
    assert!(
        t.sent()[0].text.starts_with("📅 New event\nPlanning\n\n"),
        "{}",
        t.sent()[0].text
    );
    assert!(!t.sent()[0].text.contains("Description changed"));
    assert!(!t.sent()[0].text.contains("Organizer changed"));

    assert!(!t.sent()[0].silent);
    let ignored: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE status = 'ignored'")
        .fetch_one(&t.app.pool)
        .await
        .unwrap();
    assert_eq!(ignored, 4);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn expired_token_rebootstraps_all_pages_without_replaying_history(pool: PgPool) {
    let t = test_app(pool, |_| {}, Some(local(12, 0))).await;
    connect(&t).await;
    let _agenda = upcoming(&t, vec![]).await;
    let first = sync_page(&t, vec![event("old", 14)], "expired").await;
    t.app.calendar.sync(OWNER).await.unwrap();
    drop(first);
    Mock::given(path(EVENTS))
        .and(query_param("syncToken", "expired"))
        .respond_with(ResponseTemplate::new(410))
        .expect(1)
        .with_priority(10)
        .mount(&t.server)
        .await;
    let _first_page = Mock::given(path(EVENTS))
        .and(query_param("singleEvents", "false"))
        .and(query_param_is_missing("pageToken"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"items": [event("new", 15)], "nextPageToken": "p2"})),
        )
        .with_priority(10)
        .mount_as_scoped(&t.server)
        .await;
    Mock::given(path(EVENTS))
        .and(query_param("pageToken", "p2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"items": [event("last", 16)], "nextSyncToken": "fresh"})),
        )
        .expect(1)
        .with_priority(10)
        .mount(&t.server)
        .await;
    assert_eq!(t.app.calendar.sync(OWNER).await.unwrap(), 0);
    assert_eq!(cursor(&t).await, "fresh");
    assert_eq!(event_count(&t).await, 0);
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT event_id FROM calendar_events ORDER BY event_id")
            .fetch_all(&t.app.pool)
            .await
            .unwrap();
    assert_eq!(ids, ["last", "new"]);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn a_failed_later_page_preserves_the_cursor_and_all_pending_changes(pool: PgPool) {
    let t = test_app(pool, |_| {}, Some(local(12, 0))).await;
    connect(&t).await;
    let _agenda = upcoming(&t, vec![]).await;
    let first = sync_page(&t, vec![], "s1").await;
    t.app.calendar.sync(OWNER).await.unwrap();
    drop(first);
    let _first_page = Mock::given(path(EVENTS))
        .and(query_param("syncToken", "s1"))
        .and(query_param_is_missing("pageToken"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"items": [event("new", 15)], "nextPageToken": "p2"})),
        )
        .with_priority(10)
        .mount_as_scoped(&t.server)
        .await;
    let failure = Mock::given(path(EVENTS))
        .and(query_param("pageToken", "p2"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(10)
        .mount_as_scoped(&t.server)
        .await;
    assert!(t.app.calendar.sync(OWNER).await.is_err());
    assert_eq!(cursor(&t).await, "s1");
    assert_eq!(event_count(&t).await, 0);
    drop(failure);
    let _last_page = Mock::given(path(EVENTS))
        .and(query_param("pageToken", "p2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"items": [], "nextSyncToken": "s2"})),
        )
        .with_priority(10)
        .mount_as_scoped(&t.server)
        .await;
    assert_eq!(t.app.calendar.sync(OWNER).await.unwrap(), 1);
    assert_eq!(cursor(&t).await, "s2");
    t.drain().await;
    assert_eq!(t.sent().len(), 1);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn recurring_series_uses_its_next_instance_and_keeps_it_for_cancellation(pool: PgPool) {
    let t = test_app(pool, |_| {}, Some(local(12, 0))).await;
    connect(&t).await;
    let mut master = event("series", 14);
    master["recurrence"] = json!(["RRULE:FREQ=WEEKLY"]);
    master["start"]["dateTime"] = json!((local(14, 0) - Duration::days(365)).to_rfc3339());
    master["end"]["dateTime"] = json!((local(15, 0) - Duration::days(365)).to_rfc3339());
    let mut instance = event("series_instance", 14);
    instance["recurringEventId"] = json!("series");
    let agenda = upcoming(&t, vec![instance.clone()]).await;
    let first = sync_page(&t, vec![master.clone()], "s1").await;
    t.app.calendar.sync(OWNER).await.unwrap();
    drop(first);
    master["summary"] = json!("Weekly revised");
    master["updated"] = json!("2026-11-17T02:00:00Z");
    let changed = sync_page(&t, vec![master], "s2").await;
    t.app.calendar.sync(OWNER).await.unwrap();
    t.drain().await;
    assert_eq!(t.sent().len(), 1);
    assert!(t.sent()[0].text.contains(&test_day().to_string()));
    drop(changed);
    let exception = sync_page(&t, vec![json!({"id": "series_cancelled_instance", "status": "cancelled",
        "recurringEventId": "series", "originalStartTime": {"dateTime": local(16, 0).to_rfc3339()}})], "exception").await;
    t.app.calendar.sync(OWNER).await.unwrap();
    t.drain().await;
    assert_eq!(t.sent().len(), 2);
    assert!(t.sent()[1].text.contains("Event cancelled\nWeekly revised"));
    assert!(t.sent()[1].text.contains("16:00") && t.sent()[1].text.contains("17:00"));
    drop(exception);
    drop(agenda);
    let _no_instances = upcoming(&t, vec![]).await;
    let _cancel = sync_page(
        &t,
        vec![json!({"id": "series", "status": "cancelled"})],
        "s3",
    )
    .await;
    t.app.calendar.sync(OWNER).await.unwrap();
    t.drain().await;
    assert_eq!(t.sent().len(), 3);
    assert!(t.sent()[2].text.contains("Event cancelled\nWeekly revised"));
    assert!(t.sent()[2].text.contains(&test_day().to_string()));
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn live_agenda_pages_send_a_calendar_only_digest_and_exclude_declines(pool: PgPool) {
    let t = test_app(pool, |_| {}, Some(local(9, 5))).await;
    connect(&t).await;
    let mut all_day = event("holiday", 0);
    all_day["summary"] = json!("Holiday");
    all_day["start"] = json!({"date": test_day().to_string()});
    all_day["end"] = json!({"date": (test_day() + Duration::days(1)).to_string()});
    let first = Mock::given(path(EVENTS))
        .and(query_param("singleEvents", "true"))
        .and(query_param_is_missing("pageToken"))
        .and(query_param(
            "timeMin",
            local(0, 0).with_timezone(&Utc).to_rfc3339(),
        ))
        .and(query_param(
            "timeMax",
            (local(0, 0) + Duration::days(1))
                .with_timezone(&Utc)
                .to_rfc3339(),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"items": [all_day], "nextPageToken": "p2"})),
        )
        .with_priority(10)
        .mount_as_scoped(&t.server)
        .await;
    let mut no = event("declined", 12);
    no["summary"] = json!("Do not include");
    no["attendees"] = json!([{"self": true, "responseStatus": "declined"}]);
    let second = Mock::given(path(EVENTS))
        .and(query_param("pageToken", "p2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"items": [event("meeting", 14), no]})),
        )
        .with_priority(10)
        .mount_as_scoped(&t.server)
        .await;
    t.app
        .digest
        .build(OWNER, local(9, 5).with_timezone(&Utc))
        .await
        .unwrap();
    let text = t.sent()[0].text.clone();
    assert!(text.contains("Today\n• All day"), "{text}");
    assert!(text.find("Holiday").unwrap() < text.find("Planning").unwrap());
    assert!(!text.contains("Do not include"));
    assert!(t.sent()[0].silent);
    drop(first);
    drop(second);
    t.app
        .digest
        .build(OWNER, local(9, 5).with_timezone(&Utc))
        .await
        .unwrap();
    assert_eq!(t.sent().len(), 1);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn an_account_without_calendar_consent_keeps_working_without_calendar_requests(pool: PgPool) {
    let t = test_app(pool, |_| {}, Some(local(12, 0))).await;
    t.connect_owner().await;
    assert_eq!(t.app.calendar.sync(OWNER).await.unwrap(), 0);
    t.app
        .digest
        .build(OWNER, local(9, 5).with_timezone(&Utc))
        .await
        .unwrap();
    assert!(t.sent().iter().all(|m| !m.text.contains("Today")));
    assert!(
        !t.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path().contains("calendar"))
    );
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn calendar_transport_encodes_ids_refreshes_after_401_and_keeps_page_parameters(
    pool: PgPool,
) {
    let t = test_app(pool, |_| {}, None).await;
    connect(&t).await;
    let encoded = "/calendar/v3/calendars/team%2Fcalendar@example.com/events";
    Mock::given(path(encoded))
        .and(query_param_is_missing("pageToken"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .expect(1)
        .with_priority(10)
        .mount(&t.server)
        .await;
    Mock::given(path(encoded))
        .and(query_param_is_missing("pageToken"))
        .and(query_param("syncToken", "cursor"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"items": [{"id": "first"}], "nextPageToken": "next"})),
        )
        .expect(1)
        .with_priority(10)
        .mount(&t.server)
        .await;
    Mock::given(path(encoded))
        .and(query_param("pageToken", "next"))
        .and(query_param("syncToken", "cursor"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"items": [{"id": "last"}], "nextSyncToken": "new-cursor"})),
        )
        .expect(1)
        .with_priority(10)
        .mount(&t.server)
        .await;
    let result = t
        .app
        .calendar
        .api
        .events(
            OWNER,
            "team/calendar@example.com",
            &[("syncToken", "cursor".into())],
        )
        .await
        .unwrap();
    assert_eq!(
        result.items,
        vec![json!({"id": "first"}), json!({"id": "last"})]
    );
    assert_eq!(result.next_sync_token.as_deref(), Some("new-cursor"));
    let requests = t.server.received_requests().await.unwrap();
    assert_eq!(
        requests.iter().filter(|r| r.url.path() == "/token").count(),
        2
    );
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn moving_a_series_out_of_the_window_reports_that_no_occurrence_remains(pool: PgPool) {
    let t = test_app(pool, |_| {}, Some(local(12, 0))).await;
    connect(&t).await;
    let mut master = event("series", 14);
    master["recurrence"] = json!(["RRULE:FREQ=WEEKLY"]);
    let mut instance = event("instance", 14);
    instance["recurringEventId"] = json!("series");
    let agenda = upcoming(&t, vec![instance]).await;
    let first = sync_page(&t, vec![master.clone()], "s1").await;
    t.app.calendar.sync(OWNER).await.unwrap();
    drop(first);
    drop(agenda);
    let _no_instances = upcoming(&t, vec![]).await;
    master["updated"] = json!("2026-11-17T02:00:00Z");
    master["start"]["dateTime"] = json!((local(14, 0) + Duration::days(60)).to_rfc3339());
    master["end"]["dateTime"] = json!((local(15, 0) + Duration::days(60)).to_rfc3339());
    let _changes = sync_page(&t, vec![master], "s2").await;
    t.app.calendar.sync(OWNER).await.unwrap();
    t.drain().await;
    assert_eq!(t.sent().len(), 1);
    assert!(
        t.sent()[0]
            .text
            .contains("No occurrence in the next 30 days")
    );
    assert!(
        t.sent()[0].text.contains("14:00"),
        "the old time remains visible in the diff"
    );
}
