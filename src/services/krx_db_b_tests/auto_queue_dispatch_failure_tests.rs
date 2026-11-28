//! Moved from src/db/auto_queue/entries/dispatch_failure.rs: these cases drive the KE dispatch-failure writer.

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::auto_queue::*;
use crate::services::auto_queue::transactions::record_entry_dispatch_failure_on_pg;
use sqlx::PgPool;

async fn seed_failure_entry(
    pool: &PgPool,
    run_id: &str,
    entry_id: &str,
    run_status: &str,
    retry_count: i64,
) {
    crate::db::status_check_test_support::execute_with_status_check_lifted(
        pool,
        "auto_queue_runs",
        &format!("INSERT INTO auto_queue_runs (id, status) VALUES ('{run_id}', '{run_status}')"),
    )
    .await;
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, agent_id, status, retry_count)
         VALUES ($1, $2, 'agent-1', 'dispatched', $3)",
    )
    .bind(entry_id)
    .bind(run_id)
    .bind(retry_count)
    .execute(pool)
    .await
    .expect("seed entry");
}

#[tokio::test]
async fn retry_failure_keeps_run_runnable_and_records_one_transition_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_failure_entry(&pool, "run-retry", "entry-retry", "active", 0).await;

    let result = record_entry_dispatch_failure_on_pg(&pool, "entry-retry", 3, "test_retry_failure")
        .await
        .expect("record retryable dispatch failure");
    assert!(result.changed);
    assert_eq!(result.to_status, ENTRY_STATUS_PENDING);
    assert_eq!(result.retry_count, 1);

    let state = sqlx::query_as::<_, (String, i64, String, bool)>(
        "SELECT e.status, e.retry_count, r.status, r.completed_at IS NULL
         FROM auto_queue_entries e
         JOIN auto_queue_runs r ON r.id = e.run_id
         WHERE e.id = 'entry-retry'",
    )
    .fetch_one(&pool)
    .await
    .expect("load retry state");
    assert_eq!(
        state,
        ("pending".to_string(), 1, "active".to_string(), true)
    );
    let transitions = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM auto_queue_entry_transitions
         WHERE entry_id = 'entry-retry'
           AND from_status = 'dispatched'
           AND to_status = 'pending'",
    )
    .fetch_one(&pool)
    .await
    .expect("count retry transition");
    assert_eq!(transitions, 1);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn terminal_failure_finalizes_once_and_duplicate_is_noop_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_failure_entry(&pool, "run-terminal", "entry-terminal", "active", 0).await;

    let first =
        record_entry_dispatch_failure_on_pg(&pool, "entry-terminal", 1, "test_terminal_failure")
            .await
            .expect("record terminal failure");
    let completion = sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
        "SELECT completed_at FROM auto_queue_runs WHERE id = 'run-terminal'",
    )
    .fetch_one(&pool)
    .await
    .expect("load first completion timestamp");
    let duplicate = record_entry_dispatch_failure_on_pg(
        &pool,
        "entry-terminal",
        1,
        "test_terminal_failure_duplicate",
    )
    .await
    .expect("duplicate failure is idempotent");
    let state = sqlx::query_as::<_, (String, i64, String, chrono::DateTime<chrono::Utc>, i64)>(
        "SELECT e.status, e.retry_count, r.status, r.completed_at,
                (SELECT COUNT(*) FROM auto_queue_entry_transitions
                 WHERE entry_id = e.id)
         FROM auto_queue_entries e
         JOIN auto_queue_runs r ON r.id = e.run_id
         WHERE e.id = 'entry-terminal'",
    )
    .fetch_one(&pool)
    .await
    .expect("load duplicate terminal state");
    assert!(first.changed);
    assert!(!duplicate.changed);
    assert_eq!(
        state,
        (
            "failed".to_string(),
            1,
            "completed".to_string(),
            completion,
            1,
        )
    );
    // #5993: the terminal failure is recorded as DB state only; no
    // operator card is enqueued alongside it.
    let outbox = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .expect("count outbox rows after terminal failure");
    assert_eq!(outbox, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn restoring_run_accepts_retry_without_orphaning_entry_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_failure_entry(
        &pool,
        "run-restoring-retry",
        "entry-restoring-retry",
        "restoring",
        0,
    )
    .await;

    let result = record_entry_dispatch_failure_on_pg(
        &pool,
        "entry-restoring-retry",
        3,
        "test_restoring_retry",
    )
    .await
    .expect("restore-window failure remains reducible");
    assert!(result.changed);
    assert_eq!(result.to_status, ENTRY_STATUS_PENDING);
    let state = sqlx::query_as::<_, (String, i64, String, i64)>(
        "SELECT e.status, e.retry_count, r.status,
                (SELECT COUNT(*) FROM auto_queue_entry_transitions
                 WHERE entry_id = e.id AND to_status = 'pending')
         FROM auto_queue_entries e
         JOIN auto_queue_runs r ON r.id = e.run_id
         WHERE e.id = 'entry-restoring-retry'",
    )
    .fetch_one(&pool)
    .await
    .expect("load restoring retry state");
    assert_eq!(
        state,
        ("pending".to_string(), 1, "restoring".to_string(), 1)
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn restoring_terminal_failure_retains_run_slot_ownership_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_failure_entry(
        &pool,
        "run-restoring-terminal",
        "entry-restoring-terminal",
        "restoring",
        0,
    )
    .await;
    sqlx::query(
        "INSERT INTO auto_queue_slots (
             agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
         ) VALUES ('agent-1', 0, 'run-restoring-terminal', 0, '{}'::jsonb)",
    )
    .execute(&pool)
    .await
    .expect("seed restoring slot");

    let result = record_entry_dispatch_failure_on_pg(
        &pool,
        "entry-restoring-terminal",
        1,
        "test_restoring_terminal",
    )
    .await
    .expect("terminal failure during restore");
    assert_eq!(result.to_status, ENTRY_STATUS_FAILED);
    let state = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT e.status, r.status, s.assigned_run_id
         FROM auto_queue_entries e
         JOIN auto_queue_runs r ON r.id = e.run_id
         JOIN auto_queue_slots s ON s.agent_id = e.agent_id AND s.slot_index = 0
         WHERE e.id = 'entry-restoring-terminal'",
    )
    .fetch_one(&pool)
    .await
    .expect("load restoring terminal state");
    assert_eq!(
        state,
        (
            "failed".to_string(),
            "restoring".to_string(),
            Some("run-restoring-terminal".to_string())
        )
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn non_live_runs_terminalize_entry_instead_of_blocking_dispatch_failure_pg() {
    for run_status in ["cancelled", "completed", "generated", "unknown-status"] {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let run_id = format!("run-non-live-{run_status}");
        let entry_id = format!("entry-non-live-{run_status}");
        seed_failure_entry(&pool, &run_id, &entry_id, run_status, 0).await;
        if run_status == "completed" {
            sqlx::query("UPDATE auto_queue_runs SET completed_at = NOW() WHERE id = $1")
                .bind(&run_id)
                .execute(&pool)
                .await
                .expect("stamp completed run");
        }

        let result =
            record_entry_dispatch_failure_on_pg(&pool, &entry_id, 3, "test_non_live_run_failure")
                .await
                .expect("non-live run must not block entry terminalization");
        assert_eq!(result.to_status, ENTRY_STATUS_FAILED);
        let state = sqlx::query_as::<_, (String, i64, String, i64)>(
            "SELECT e.status, e.retry_count, r.status,
                    (SELECT COUNT(*) FROM auto_queue_entry_transitions
                     WHERE entry_id = e.id AND to_status = 'failed')
             FROM auto_queue_entries e
             JOIN auto_queue_runs r ON r.id = e.run_id
             WHERE e.id = $1",
        )
        .bind(&entry_id)
        .fetch_one(&pool)
        .await
        .expect("load non-live terminal state");
        let expected_run_status = if run_status == "generated" {
            // An eligible non-live generated run can be finalized once its
            // last entry becomes terminal. Unknown or terminal states are
            // intentionally preserved by the run-status CAS.
            "completed"
        } else {
            run_status
        };
        assert_eq!(
            state,
            ("failed".to_string(), 1, expected_run_status.to_string(), 1),
            "run status {run_status} must not block entry terminalization"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}

#[tokio::test]
async fn concurrent_failure_has_one_cas_winner_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    seed_failure_entry(&pool, "run-race", "entry-race", "active", 0).await;

    let left_pool = pool.clone();
    let right_pool = pool.clone();
    let (left, right) = tokio::join!(
        async move {
            record_entry_dispatch_failure_on_pg(&left_pool, "entry-race", 3, "race_left").await
        },
        async move {
            record_entry_dispatch_failure_on_pg(&right_pool, "entry-race", 3, "race_right").await
        },
    );
    let left = left.expect("left failure invocation");
    let right = right.expect("right failure invocation");
    assert_eq!(
        [left.changed, right.changed]
            .into_iter()
            .filter(|changed| *changed)
            .count(),
        1
    );
    let state = sqlx::query_as::<_, (String, i64, String, i64)>(
        "SELECT e.status, e.retry_count, r.status,
                (SELECT COUNT(*) FROM auto_queue_entry_transitions
                 WHERE entry_id = e.id)
         FROM auto_queue_entries e
         JOIN auto_queue_runs r ON r.id = e.run_id
         WHERE e.id = 'entry-race'",
    )
    .fetch_one(&pool)
    .await
    .expect("load raced failure state");
    assert_eq!(state, ("pending".to_string(), 1, "active".to_string(), 1));

    pool.close().await;
    pg_db.drop().await;
}
