use crate::services::auto_queue::route::AutoQueueActivateDeps;
use crate::services::auto_queue::route::planning::record_entry_dispatch_failure;
use std::sync::Arc;

fn must_ok<T, E: std::fmt::Debug>(result: Result<T, E>, context: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("{context}: {error:?}"), // agentdesk-audit: allow-unwrap — test-only helper in #[cfg(test)] module
    }
}

#[tokio::test]
async fn production_failure_wrapper_preserves_restoring_slot_binding_pg() {
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    must_ok(
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, status, max_concurrent_threads)
                 VALUES ('run-restore-wrapper', 'restoring', 1)",
        )
        .execute(&pool)
        .await,
        "seed restoring run",
    );
    must_ok(
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                     id, run_id, agent_id, status, retry_count, slot_index, thread_group
                 ) VALUES (
                     'entry-restore-wrapper', 'run-restore-wrapper', 'agent-1',
                     'dispatched', 0, 0, 0
                 )",
        )
        .execute(&pool)
        .await,
        "seed restoring dispatched entry",
    );
    must_ok(
        sqlx::query(
            "INSERT INTO auto_queue_slots (
                     agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
                 ) VALUES ('agent-1', 0, 'run-restore-wrapper', 0, '{}'::jsonb)",
        )
        .execute(&pool)
        .await,
        "seed restoring slot",
    );
    let config = crate::config::Config::default();
    let engine = must_ok(
        crate::engine::PolicyEngine::new_with_pg(
            &config,
            Some(pool.clone()),
            crate::services::discord::health::slot_row_host_refusal,
        ),
        "create test policy engine",
    );
    let deps = AutoQueueActivateDeps {
        pg_pool: Some(pool.clone()),
        slot_row_guard: engine.slot_row_guard(),
        engine,
        config: Arc::new(config),
        health_registry: None,
        guild_id: None,
    };

    let result = must_ok(
        record_entry_dispatch_failure(
            &deps,
            "entry-restore-wrapper",
            "restore_run_create_dispatch_failed",
        ),
        "production failure wrapper",
    );
    assert_eq!(
        result.to_status,
        crate::db::auto_queue::ENTRY_STATUS_PENDING
    );
    let state = must_ok(
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT e.status, r.status, s.assigned_run_id
                 FROM auto_queue_entries e
                 JOIN auto_queue_runs r ON r.id = e.run_id
                 JOIN auto_queue_slots s ON s.agent_id = e.agent_id AND s.slot_index = 0
                 WHERE e.id = 'entry-restore-wrapper'",
        )
        .fetch_one(&pool)
        .await,
        "load production-wrapper state",
    );
    assert_eq!(
        state,
        (
            "pending".to_string(),
            "restoring".to_string(),
            Some("run-restore-wrapper".to_string()),
        )
    );

    pool.close().await;
    pg_db.drop().await;
}
