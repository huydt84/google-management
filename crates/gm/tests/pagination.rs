mod common;

use common::*;
use sqlx::PgPool;

async fn seed(pool: &PgPool, count: i64) {
    sqlx::query(
        "INSERT INTO events (id, account, source, kind, external_id, status, data)
         SELECT n, $1, CASE WHEN n = 100 THEN 'calendar' ELSE 'gmail' END,
                'received', n::text, 'ignored', '{}'::jsonb
         FROM generate_series(1, $2::bigint) AS n",
    )
    .bind(OWNER)
    .bind(count)
    .execute(pool)
    .await
    .unwrap();
}

fn ids(body: &str) -> Vec<i64> {
    regex::Regex::new(r#"id="event-(\d+)""#)
        .unwrap()
        .captures_iter(body)
        .map(|c| c[1].parse().unwrap())
        .collect()
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn dashboard_pages_in_both_directions_without_shifting_for_new_events(pool: PgPool) {
    seed(&pool, 121).await;
    let t = test_app(pool.clone(), |_| {}, None).await;
    let router = gm::web::router(t.app.state.clone());
    let cookie = session_cookie(&t, OWNER);

    let (status, _, first) = send(&router, get("/", Some(&cookie))).await;
    assert_eq!(status, 200);
    assert_eq!(ids(&first), (72..=121).rev().collect::<Vec<_>>());
    assert!(first.contains(r#"href="/?before=72#events-title""#));
    assert!(!first.contains("rel=\"prev\""));

    let (_, _, middle) = send(&router, get("/?before=72", Some(&cookie))).await;
    assert_eq!(ids(&middle), (22..=71).rev().collect::<Vec<_>>());
    assert!(middle.contains(r#"href="/?after=71#events-title""#));
    assert!(middle.contains(r#"href="/?before=22#events-title""#));
    assert!(middle.contains(r#"action="/events/71/feedback?before=72""#));
    assert!(middle.contains(r#"action="/events/71/eval?before=72""#));

    sqlx::query("INSERT INTO events (id, account, source, kind, external_id) VALUES (122, $1, 'gmail', 'received', 'new-arrival')")
        .bind(OWNER).execute(&pool).await.unwrap();
    let (_, _, unchanged) = send(&router, get("/?before=72", Some(&cookie))).await;
    assert_eq!(ids(&unchanged), ids(&middle));

    let (_, _, last) = send(&router, get("/?before=22", Some(&cookie))).await;
    assert_eq!(ids(&last), (1..=21).rev().collect::<Vec<_>>());
    assert!(!last.contains("rel=\"next\""));
    let (_, _, back) = send(&router, get("/?after=21", Some(&cookie))).await;
    assert_eq!(ids(&back), ids(&middle));
    let (_, _, back) = send(&router, get("/?after=71", Some(&cookie))).await;
    assert_eq!(ids(&back), ids(&first));
    assert!(back.contains(r#"href="/?after=121#events-title""#));

    // Cursor rows need not survive retention for their links to keep working.
    sqlx::query("DELETE FROM events WHERE id = 72")
        .execute(&pool)
        .await
        .unwrap();
    let (_, _, back) = send(&router, get("/?before=72", Some(&cookie))).await;
    assert_eq!(ids(&back), ids(&middle));
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn pagination_handles_empty_exact_and_invalid_pages(pool: PgPool) {
    let t = test_app(pool.clone(), |_| {}, None).await;
    let router = gm::web::router(t.app.state.clone());
    let cookie = session_cookie(&t, OWNER);
    let (_, _, empty) = send(&router, get("/", Some(&cookie))).await;
    assert!(empty.contains("No events yet."));
    assert!(!empty.contains("class=\"pagination\""));

    seed(&pool, 50).await;
    let (_, _, exact) = send(&router, get("/", Some(&cookie))).await;
    assert_eq!(ids(&exact).len(), 50);
    assert!(!exact.contains("class=\"pagination\""));
    for uri in ["/?before=1", "/?after=9223372036854775807"] {
        let (status, _, empty) = send(&router, get(uri, Some(&cookie))).await;
        assert_eq!(status, 200);
        assert!(ids(&empty).is_empty());
        assert!(empty.contains("No events on this page."));
        assert!(empty.contains(r#"href="/#events-title""#));
    }
    for uri in [
        "/?before=0",
        "/?after=-1",
        "/?before=2&after=1",
        "/?before=abc",
        "/?after=9223372036854775808",
    ] {
        assert_eq!(send(&router, get(uri, Some(&cookie))).await.0, 400, "{uri}");
    }
}
