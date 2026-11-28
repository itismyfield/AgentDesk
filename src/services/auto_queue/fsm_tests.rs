use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::auto_queue::route::AutoQueueActivateDeps;
use crate::services::auto_queue::route::fsm::attempt_restore_dispatch;
use crate::services::auto_queue::route::view::{RestoreDispatchCandidate, RestoreEntryRecord};
use crate::services::settings::clamp_retry_limit;
use crate::test_support::wait::poll_within;
use sqlx::{Connection, PgConnection, PgPool};
use std::sync::Arc;

#[test]
fn clamp_retry_limit_bounds() {
    // Minimum boundary: 0 should be clamped to 1
    assert_eq!(clamp_retry_limit(0), 1);

    // Normal values should remain unchanged
    assert_eq!(clamp_retry_limit(1), 1);
    assert_eq!(clamp_retry_limit(3), 3);
    assert_eq!(clamp_retry_limit(10), 10);

    // Maximum boundary: > i64::MAX should be clamped to i64::MAX
    assert_eq!(clamp_retry_limit(i64::MAX as u64), i64::MAX);
    assert_eq!(clamp_retry_limit(i64::MAX as u64 + 10), i64::MAX);
    assert_eq!(clamp_retry_limit(u64::MAX), i64::MAX);
}

async fn setup_restore_candidate(pool: &PgPool, with_existing_dispatch: bool) {
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('fsm-gate-agent', 'FSM Gate Agent', 'claude', 'fsm-gate-channel')",
    )
    .execute(pool)
    .await
    .expect("seed FSM gate agent");
    sqlx::query(
        "INSERT INTO auto_queue_runs
                (id, repo, agent_id, status, max_concurrent_threads)
             VALUES ('fsm-gate-run', 'fsm-gate-repo', 'fsm-gate-agent', 'active', 1)",
    )
    .execute(pool)
    .await
    .expect("seed FSM gate run");
    sqlx::query(
        "INSERT INTO kanban_cards
                (id, title, status, assigned_agent_id, metadata)
             VALUES (
                'fsm-gate-card', 'FSM Gate Card', 'in_progress', 'fsm-gate-agent',
                '{\"sandbox_preflight\":true,\"production_mutation_allowed\":false}'::jsonb
             )",
    )
    .execute(pool)
    .await
    .expect("seed FSM gate card");
    sqlx::query(
        "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES (
                'fsm-gate-entry', 'fsm-gate-run', 'fsm-gate-card',
                'fsm-gate-agent', 'pending', 0
             )",
    )
    .execute(pool)
    .await
    .expect("seed FSM gate entry");
    if with_existing_dispatch {
        seed_recovered_dispatch(pool, "fsm-existing-dispatch").await;
    }
}

async fn seed_recovered_dispatch(pool: &PgPool, dispatch_id: &str) {
    sqlx::query(
        "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
             VALUES (
                $1, 'fsm-gate-card', 'fsm-gate-agent',
                'implementation', 'dispatched', 'Recovered dispatch'
             )",
    )
    .bind(dispatch_id)
    .execute(pool)
    .await
    .expect("seed FSM recovered dispatch");
    sqlx::query(
        "UPDATE kanban_cards
             SET latest_dispatch_id = $1
             WHERE id = 'fsm-gate-card'",
    )
    .bind(dispatch_id)
    .execute(pool)
    .await
    .expect("link FSM recovered dispatch");
}

fn restore_deps(pool: &PgPool) -> AutoQueueActivateDeps {
    let config = crate::config::Config::default();
    let engine = crate::engine::PolicyEngine::new_with_pg(
        &config,
        Some(pool.clone()),
        crate::services::discord::health::slot_row_host_refusal,
    )
    .expect("create FSM gate policy engine");
    AutoQueueActivateDeps {
        pg_pool: Some(pool.clone()),
        slot_row_guard: engine.slot_row_guard(),
        engine,
        config: Arc::new(config),
        health_registry: None,
        guild_id: None,
    }
}

fn restore_candidate() -> RestoreDispatchCandidate {
    RestoreDispatchCandidate {
        entry: RestoreEntryRecord {
            entry_id: "fsm-gate-entry".to_string(),
            card_id: "fsm-gate-card".to_string(),
            agent_id: "fsm-gate-agent".to_string(),
            thread_group: 0,
        },
        title: "FSM Gate Card".to_string(),
    }
}

async fn begin_run_token_holder(database_url: &str) -> PgConnection {
    let mut conn = PgConnection::connect(database_url)
        .await
        .expect("connect FSM run-token holder");
    sqlx::query("BEGIN")
        .execute(&mut conn)
        .await
        .expect("begin FSM run-token holder");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || 'fsm-gate-run'))")
        .execute(&mut conn)
        .await
        .expect("hold FSM run token");
    conn
}

async fn run_token_waiter_sample(conn: &mut PgConnection) -> Result<(), String> {
    sqlx::query("SELECT pg_stat_clear_snapshot()")
        .execute(&mut *conn)
        .await
        .expect("clear FSM backend-status snapshot");
    let waiting = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
                 SELECT 1
                 FROM pg_stat_activity
                 WHERE datname = current_database()
                   AND wait_event_type = 'Lock'
                   AND query LIKE '%pg_advisory_xact_lock%aq_run:%'
             )",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("inspect FSM run-token waiter");
    if waiting {
        Ok(())
    } else {
        Err("FSM restore path did not wait for the run token".into())
    }
}

async fn cancel_run_and_release_token(conn: &mut PgConnection) {
    sqlx::query(
        "UPDATE auto_queue_runs
             SET status = 'cancelled', completed_at = NOW()
             WHERE id = 'fsm-gate-run'",
    )
    .execute(&mut *conn)
    .await
    .expect("cancel FSM gate run");
    sqlx::query("COMMIT")
        .execute(&mut *conn)
        .await
        .expect("commit FSM gate cancellation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_attach_existing_path_rechecks_cancelled_run_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_restore_candidate(&pool, true).await;
    let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;
    let deps = restore_deps(&pool);
    let attempt = tokio::task::spawn_blocking(move || {
        attempt_restore_dispatch(&deps, "fsm-gate-run", &restore_candidate())
    });

    poll_within(
        async || run_token_waiter_sample(&mut token_holder).await,
        std::time::Duration::from_secs(5),
        "PostgreSQL lock waiter",
    )
    .await
    .unwrap();
    cancel_run_and_release_token(&mut token_holder).await;
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), attempt)
        .await
        .expect("FSM attach-existing attempt completes")
        .expect("join FSM attach-existing attempt")
        .expect_err("FSM attach-existing path must reject a cancelled run");
    assert!(error.contains("fsm-gate-run is cancelled"), "{error}");
    let entry_status: String =
        sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'fsm-gate-entry'")
            .fetch_one(&pool)
            .await
            .expect("load rejected FSM attach-existing entry");
    assert_eq!(entry_status, crate::db::auto_queue::ENTRY_STATUS_PENDING);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_reattach_path_rechecks_cancelled_run_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    setup_restore_candidate(&pool, false).await;
    let mut token_holder = begin_run_token_holder(&pg_db.database_url).await;
    let deps = restore_deps(&pool);
    let attempt = tokio::task::spawn_blocking(move || {
        attempt_restore_dispatch(&deps, "fsm-gate-run", &restore_candidate())
    });

    poll_within(
        async || run_token_waiter_sample(&mut token_holder).await,
        std::time::Duration::from_secs(5),
        "PostgreSQL lock waiter",
    )
    .await
    .unwrap();
    seed_recovered_dispatch(&pool, "fsm-recovered-dispatch").await;
    cancel_run_and_release_token(&mut token_holder).await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), attempt)
        .await
        .expect("FSM recovery reattach attempt completes")
        .expect("join FSM recovery reattach attempt")
        .expect("FSM recovery path reports a rejected reattach as not dispatched");
    assert!(!result.dispatched);
    let entry_status: String =
        sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'fsm-gate-entry'")
            .fetch_one(&pool)
            .await
            .expect("load rejected FSM recovery entry");
    assert_eq!(entry_status, crate::db::auto_queue::ENTRY_STATUS_PENDING);

    pool.close().await;
    pg_db.drop().await;
}
