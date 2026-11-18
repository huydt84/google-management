use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::Utc;
use gm::bus::{Bus, Outcome, backoff_delay};
use serde_json::json;
use sqlx::PgPool;

async fn job(pool: &PgPool) -> (String, i32, Option<String>) {
    sqlx::query_as("SELECT status, attempts, last_error FROM jobs ORDER BY id DESC LIMIT 1")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test]
fn backoff_doubles_and_caps() {
    assert_eq!(backoff_delay(1).num_seconds(), 5);
    assert_eq!(backoff_delay(2).num_seconds(), 10);
    assert_eq!(backoff_delay(20).num_seconds(), 900);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn publish_is_transactional_and_delivers_once_per_subscriber(pool: PgPool) {
    let bus = Bus::new(pool.clone(), 3);
    let hits = Arc::new(AtomicUsize::new(0));
    for name in ["a", "b"] {
        let h = hits.clone();
        bus.subscribe("t", name, move |_, _| {
            let h = h.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                Ok(Outcome::Done)
            }
        });
    }
    // Rolled back: nothing delivered.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(bus.publish(&mut tx, "t", &json!({})).await.unwrap(), 2);
    tx.rollback().await.unwrap();
    assert!(!bus.run_once().await.unwrap());

    let mut tx = pool.begin().await.unwrap();
    bus.publish(&mut tx, "t", &json!({"x": 1})).await.unwrap();
    tx.commit().await.unwrap();
    assert!(bus.run_once().await.unwrap());
    assert!(bus.run_once().await.unwrap());
    assert!(!bus.run_once().await.unwrap());
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn failures_retry_with_backoff_then_die(pool: PgPool) {
    let bus = Bus::new(pool.clone(), 2);
    bus.subscribe("t", "fails", |_, _| async { anyhow::bail!("boom") });
    let mut conn = pool.acquire().await.unwrap();
    bus.publish(&mut conn, "t", &json!({})).await.unwrap();

    assert!(bus.run_once().await.unwrap());
    let (status, attempts, err) = job(&pool).await;
    assert_eq!((status.as_str(), attempts), ("pending", 1));
    assert!(err.unwrap().contains("boom"));
    assert!(!bus.run_once().await.unwrap(), "backoff delays the retry");

    sqlx::query("UPDATE jobs SET run_after = now()")
        .execute(&pool)
        .await
        .unwrap();
    assert!(bus.run_once().await.unwrap());
    assert_eq!(job(&pool).await.0, "dead");
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn defer_does_not_use_an_attempt(pool: PgPool) {
    let bus = Bus::new(pool.clone(), 1);
    let at = Utc::now() + chrono::Duration::hours(3);
    bus.subscribe(
        "t",
        "later",
        move |_, _| async move { Ok(Outcome::Defer(at)) },
    );
    let mut conn = pool.acquire().await.unwrap();
    bus.publish(&mut conn, "t", &json!({})).await.unwrap();
    assert!(bus.run_once().await.unwrap());
    let (status, attempts, _) = job(&pool).await;
    assert_eq!((status.as_str(), attempts), ("pending", 0));
    let run_after: chrono::DateTime<Utc> = sqlx::query_scalar("SELECT run_after FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!((run_after - at).num_seconds().abs() <= 1);
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn stale_running_jobs_are_requeued(pool: PgPool) {
    let bus = Bus::new(pool.clone(), 3);
    bus.subscribe("t", "s", |_, _| async { Ok(Outcome::Done) });
    let recent = Utc::now() - chrono::Duration::seconds(1);
    sqlx::query("INSERT INTO jobs (topic, subscriber, payload, status, locked_at) VALUES ('t','s','{}','running', NULL), ('t','s','{}','running', $1), ('t','s','{}','running', $2)")
        .bind(recent - chrono::Duration::hours(1))
        .bind(recent)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        bus.requeue_stale(chrono::Duration::minutes(10))
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        bus.requeue_stale(chrono::Duration::zero()).await.unwrap(),
        1
    );
}
