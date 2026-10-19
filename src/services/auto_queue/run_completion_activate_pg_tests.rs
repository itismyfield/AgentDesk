//! The activate tail and the empty-run short-circuit must obey the same
//! completion gates as every other derived entry point.

use super::activate_command::{complete_run_if_empty, finalize_activate_run_and_build_response};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::auto_queue::AutoQueueLogContext;
use axum::http::StatusCode;
use sqlx::PgPool;

async fn setup_pool(pg_db: &TestPostgresDb) -> PgPool {
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('agent-1', 'Agent 1', 'claude', '123')",
    )
    .execute(&pool)
    .await
    .expect("seed agent");
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-1', 'repo-1', 'agent-1', 'active')",
    )
    .execute(&pool)
    .await
    .expect("seed run");
    sqlx::query(
        "INSERT INTO auto_queue_slots
            (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
         VALUES ('agent-1', 0, 'run-1', 0, CAST('{}' AS jsonb))",
    )
    .execute(&pool)
    .await
    .expect("seed slot");
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
         VALUES ('card-1', 'Card 1', 'in_progress', 'agent-1')",
    )
    .execute(&pool)
    .await
    .expect("seed card");
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status)
         VALUES ('entry-1', 'run-1', 'card-1', 'agent-1', 'failed')",
    )
    .execute(&pool)
    .await
    .expect("seed entry");
    pool
}

async fn run_status(pool: &PgPool, run_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(pool)
        .await
        .expect("run status")
}

async fn slot_run(pool: &PgPool) -> Option<String> {
    sqlx::query_scalar(
        "SELECT assigned_run_id FROM auto_queue_slots WHERE agent_id = 'agent-1' AND slot_index = 0",
    )
    .fetch_one(pool)
    .await
    .expect("slot assignment")
}

async fn outbox_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM message_outbox")
        .fetch_one(pool)
        .await
        .expect("outbox count")
}

/// Contract: a drained activate defers while the phase-gate grace window is
/// open and, once it closes, completes with exactly one notification.
#[tokio::test]
async fn activate_tail_obeys_grace_window_and_notifies_once_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db).await;
    let ctx = AutoQueueLogContext::new().run("run-1");
    sqlx::query(
        "UPDATE auto_queue_runs SET phase_gate_grace_until = NOW() + INTERVAL '30 seconds'
         WHERE id = 'run-1'",
    )
    .execute(&pool)
    .await
    .expect("open grace window");

    let (status, _) =
        finalize_activate_run_and_build_response(&pool, "run-1", &ctx, vec![], vec![])
            .await
            .expect("activate response during grace");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(run_status(&pool, "run-1").await, "active");
    assert_eq!(slot_run(&pool).await.as_deref(), Some("run-1"));
    assert_eq!(outbox_count(&pool).await, 0);

    sqlx::query("UPDATE auto_queue_runs SET phase_gate_grace_until = NULL WHERE id = 'run-1'")
        .execute(&pool)
        .await
        .expect("close grace window");
    for _ in 0..2 {
        let (status, _) =
            finalize_activate_run_and_build_response(&pool, "run-1", &ctx, vec![], vec![])
                .await
                .expect("activate response after grace");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run_status(&pool, "run-1").await, "completed");
        assert_eq!(slot_run(&pool).await, None);
        assert_eq!(outbox_count(&pool).await, 1);
    }

    pool.close().await;
    pg_db.drop().await;
}

/// Contract: the empty-run short-circuit completes only through the canonical
/// writer and reports an unchanged terminal run instead of claiming completion.
#[tokio::test]
async fn empty_run_short_circuit_reports_real_outcome_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = setup_pool(&pg_db).await;
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-empty', 'repo-1', 'agent-1', 'pending'),
                ('run-gone', 'repo-1', 'agent-1', 'cancelled')",
    )
    .execute(&pool)
    .await
    .expect("seed empty runs");

    let ctx = AutoQueueLogContext::new().run("run-empty");
    let (status, body) = complete_run_if_empty(&pool, "run-empty", &ctx)
        .await
        .expect_err("empty run short-circuits");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["message"],
        "Stale empty run completed — no entries to dispatch"
    );
    assert_eq!(run_status(&pool, "run-empty").await, "completed");
    assert_eq!(outbox_count(&pool).await, 0);

    let ctx = AutoQueueLogContext::new().run("run-gone");
    let (status, body) = complete_run_if_empty(&pool, "run-gone", &ctx)
        .await
        .expect_err("terminal empty run short-circuits");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["message"],
        "Stale empty run left unchanged (already_terminal) — no entries to dispatch"
    );
    assert_eq!(run_status(&pool, "run-gone").await, "cancelled");

    assert!(
        complete_run_if_empty(&pool, "run-1", &AutoQueueLogContext::new().run("run-1"))
            .await
            .is_ok(),
        "a run with entries proceeds to dispatch"
    );

    pool.close().await;
    pg_db.drop().await;
}
