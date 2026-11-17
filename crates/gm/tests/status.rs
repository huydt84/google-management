//! `gm status` database checks: migrations (pending, edited after being applied, unknown) and the
//! queue. The launchd and endpoint checks read the live Mac and are covered by unit tests of their
//! parsers (`src/status.rs`).
use gm::status::{State, migrations, queue};
use sqlx::PgPool;

async fn state(pool: &PgPool) -> (State, String) {
    let c = migrations(pool).await.unwrap();
    (c.state, c.detail)
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn migrations_up_to_date_pending_edited_and_unknown(pool: PgPool) {
    assert_eq!(state(&pool).await.0, State::Ok);

    let latest: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
        .bind(latest)
        .execute(&pool)
        .await
        .unwrap();
    let (s, detail) = state(&pool).await;
    assert_eq!(s, State::Warn);
    assert!(detail.contains(&format!("[{latest}]")), "{detail}");

    // An applied migration whose file changed afterwards: sqlx would refuse to start.
    sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = 1")
        .execute(&pool)
        .await
        .unwrap();
    let (s, detail) = state(&pool).await;
    assert_eq!(s, State::Fail);
    assert!(detail.contains("edited"), "{detail}");

    sqlx::query("UPDATE _sqlx_migrations SET checksum = (SELECT checksum FROM _sqlx_migrations WHERE version = 2) WHERE version = 1")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (999, 'from a newer build', true, '\\x00', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (s, _) = state(&pool).await;
    assert_eq!(s, State::Fail, "an edited or unknown migration fails");
}

#[sqlx::test(migrator = "gm::db::MIGRATOR")]
async fn queue_reports_dead_jobs_and_open_incidents(pool: PgPool) {
    let checks = queue(&pool, None).await.unwrap();
    assert_eq!(checks[0].state, State::Ok, "{checks:?}");
    sqlx::query(
        "INSERT INTO jobs (topic, subscriber, payload, status) VALUES ('t', 's', '{}', 'dead')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO health_state (check_name, failing_since, detail) VALUES ('model_down', now(), 'Model down')")
        .execute(&pool)
        .await
        .unwrap();
    let checks = queue(&pool, None).await.unwrap();
    assert_eq!(checks[0].state, State::Fail);
    let health = checks.iter().find(|c| c.name == "health").unwrap();
    assert_eq!(
        (health.state, health.detail.as_str()),
        (State::Fail, "Model down")
    );
}
