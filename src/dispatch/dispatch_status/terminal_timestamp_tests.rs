use super::set_dispatch_status_on_pg_async;
use crate::dispatch::test_support::{DispatchPostgresTestDb, seed_pg_dispatch};
use chrono::{DateTime, Utc};
use serde_json::json;

#[tokio::test]
async fn postgres_terminal_timestamps_preserve_first_finish_and_respect_touch_flag() {
    let db = DispatchPostgresTestDb::create("dispatch_finish", "terminal timestamps").await;
    let pool = db.connect_and_migrate().await;
    let result = json!({"summary": "finished"});

    // Cover both SQL branches (with and without a result), every terminal
    // status, a non-terminal status, and callers that own timestamp handling.
    for with_result in [false, true] {
        for status in ["completed", "failed", "cancelled", "dispatched"] {
            for touch in [false, true] {
                let id = format!("{status}-{with_result}-{touch}");
                seed_pg_dispatch(&pool, &id, "timestamp regression").await;
                assert_eq!(
                    set_dispatch_status_on_pg_async(
                        &pool,
                        &id,
                        status,
                        with_result.then_some(&result),
                        "test_terminal_timestamp",
                        Some(&["pending"]),
                        touch,
                    )
                    .await
                    .expect("transition dispatch"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
                    1
                );
                let timestamp: Option<DateTime<Utc>> =
                    sqlx::query_scalar("SELECT completed_at FROM task_dispatches WHERE id = $1")
                        .bind(&id)
                        .fetch_one(&pool)
                        .await
                        .expect("read first timestamp"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
                assert_eq!(timestamp.is_some(), touch && status != "dispatched", "{id}");

                // A guarded-out transition must leave the timestamp alone.
                assert_eq!(
                    set_dispatch_status_on_pg_async(
                        &pool,
                        &id,
                        "completed",
                        None,
                        "test_guard",
                        Some(&["pending"]),
                        true,
                    )
                    .await
                    .expect("guard transition"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
                    0
                );
                let preserved: Option<DateTime<Utc>> =
                    sqlx::query_scalar("SELECT completed_at FROM task_dispatches WHERE id = $1")
                        .bind(&id)
                        .fetch_one(&pool)
                        .await
                        .expect("read guarded timestamp"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
                assert_eq!(preserved, timestamp);

                if timestamp.is_some() {
                    // A known earlier finish proves that an idempotent rewrite
                    // cannot replace the original time with a fresh NOW().
                    let first_finish = DateTime::from_timestamp(1_700_000_000, 0)
                        .expect("valid fixture timestamp"); // agentdesk-audit: allow-unwrap — constant test fixture
                    sqlx::query("UPDATE task_dispatches SET completed_at = $1 WHERE id = $2")
                        .bind(first_finish)
                        .bind(&id)
                        .execute(&pool)
                        .await
                        .expect("seed original finish"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
                    set_dispatch_status_on_pg_async(
                        &pool,
                        &id,
                        status,
                        with_result.then_some(&result),
                        "test_repeated_finish",
                        None,
                        true,
                    )
                    .await
                    .expect("repeat terminal transition"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
                    let repeated: Option<DateTime<Utc>> = sqlx::query_scalar(
                        "SELECT completed_at FROM task_dispatches WHERE id = $1",
                    )
                    .bind(&id)
                    .fetch_one(&pool)
                    .await
                    .expect("read repeated finish"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
                    assert_eq!(repeated, Some(first_finish));
                }
            }
        }
    }

    pool.close().await;
    db.drop().await;
}
