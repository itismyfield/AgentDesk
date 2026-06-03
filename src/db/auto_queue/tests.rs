#![cfg(test)]

#[allow(unused_imports)]
use super::entries::resume_session_id_from_context;
#[allow(unused_imports)]
use super::*;

#[cfg(test)]
mod resume_session_context_tests {
    use super::resume_session_id_from_context;

    #[test]
    fn resume_session_id_from_context_prefers_retry_field_and_trims() {
        assert_eq!(
            resume_session_id_from_context(Some(
                r#"{"auto_queue_retry_resume_session_id":" thread-1585 ","resume_session_id":"old"}"#,
            ))
            .as_deref(),
            Some("thread-1585")
        );
        assert_eq!(
            resume_session_id_from_context(Some(r#"{"resume_session_id":" fallback-thread "}"#))
                .as_deref(),
            Some("fallback-thread")
        );
        assert_eq!(
            resume_session_id_from_context(Some(r#"{"auto_queue_retry_resume_session_id":"   "}"#)),
            None
        );
    }
}

#[cfg(test)]
mod dispatch_terminal_sync_pg_tests {
    use super::{
        ENTRY_STATUS_DONE, ENTRY_STATUS_FAILED, ENTRY_STATUS_SKIPPED, ENTRY_STATUS_USER_CANCELLED,
        EntryStatusUpdateOptions, PhaseGateStateWrite, SlotAllocation,
        allocate_slot_for_group_agent_pg, clear_phase_gate_state_on_pg,
        finalize_completed_dispatch_terminal_entry_on_pg_tx, save_phase_gate_state_on_pg,
        slot_has_active_dispatch_pg, slot_has_recent_terminal_auto_queue_dispatch_pg,
        sync_dispatch_terminal_entries_on_pg_tx, update_entry_status_on_pg,
    };
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use chrono::{DateTime, Utc};
    use sqlx::{Connection, PgConnection, PgPool, Row};

    async fn setup_pool(pg_db: &TestPostgresDb) -> PgPool {
        // #2048 F3 needs >=2 concurrent connections (advisory-lock conn +
        // inner allocator); the lean default test pool of 1 deadlocked.
        let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-1', 'repo-1', 'agent-1', 'active')",
        )
        .execute(&pool)
        .await
        .expect("seed run");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-1', 'Agent 1', 'claude', '123')",
        )
        .execute(&pool)
        .await
        .expect("seed agent");
        sqlx::query(
            "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
             VALUES ('agent-1', 0, 'run-1', 0, CAST('{}' AS jsonb))",
        )
        .execute(&pool)
        .await
        .expect("seed slot");
        pool
    }

    async fn entry_row_status_dispatch_completed(
        pool: &PgPool,
        entry_id: &str,
    ) -> (String, Option<String>, Option<DateTime<Utc>>) {
        let row = sqlx::query(
            "SELECT status, dispatch_id, completed_at
             FROM auto_queue_entries
             WHERE id = $1",
        )
        .bind(entry_id)
        .fetch_one(pool)
        .await
        .expect("entry row");
        (
            row.try_get::<String, _>("status").expect("status"),
            row.try_get::<Option<String>, _>("dispatch_id")
                .expect("dispatch_id"),
            row.try_get::<Option<DateTime<Utc>>, _>("completed_at")
                .expect("completed_at"),
        )
    }

    async fn run_status(pool: &PgPool, run_id: &str) -> String {
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .expect("run row")
    }

    async fn slot_run(pool: &PgPool, agent_id: &str, slot_index: i64) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT assigned_run_id
             FROM auto_queue_slots
             WHERE agent_id = $1 AND slot_index = $2",
        )
        .bind(agent_id)
        .bind(slot_index)
        .fetch_one(pool)
        .await
        .expect("slot row")
    }

    async fn slot_group(pool: &PgPool, agent_id: &str, slot_index: i64) -> Option<i64> {
        sqlx::query_scalar::<_, Option<i64>>(
            "SELECT assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = $1 AND slot_index = $2",
        )
        .bind(agent_id)
        .bind(slot_index)
        .fetch_one(pool)
        .await
        .expect("slot row")
    }

    async fn seed_active_slot_dispatch(pool: &PgPool, dispatch_id: &str, slot_index: i64) {
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ($1, 'agent-1', 'dispatched', $2)",
        )
        .bind(dispatch_id)
        .bind(
            serde_json::json!({
                "auto_queue": true,
                "slot_index": slot_index
            })
            .to_string(),
        )
        .execute(pool)
        .await
        .expect("seed active slot dispatch");
    }

    async fn seed_active_slot_dispatch_on_conn(
        conn: &mut PgConnection,
        dispatch_id: &str,
        slot_index: i64,
    ) {
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ($1, 'agent-1', 'dispatched', $2)",
        )
        .bind(dispatch_id)
        .bind(
            serde_json::json!({
                "auto_queue": true,
                "slot_index": slot_index
            })
            .to_string(),
        )
        .execute(&mut *conn)
        .await
        .expect("seed active slot dispatch");
    }

    async fn lock_slot_row_on_conn(
        database_url: &str,
        agent_id: &str,
        slot_index: i64,
    ) -> PgConnection {
        let mut conn = PgConnection::connect(database_url)
            .await
            .expect("connect slot lock connection");
        sqlx::query("BEGIN")
            .execute(&mut conn)
            .await
            .expect("begin slot lock tx");
        sqlx::query(
            "SELECT 1
             FROM auto_queue_slots
             WHERE agent_id = $1 AND slot_index = $2
             FOR UPDATE",
        )
        .bind(agent_id)
        .bind(slot_index)
        .fetch_one(&mut conn)
        .await
        .expect("lock slot row");
        conn
    }

    async fn wait_for_blocked_slot_update(conn: &mut PgConnection, query_fragment: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let blocked = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1
                     FROM pg_stat_activity
                     WHERE datname = current_database()
                       AND wait_event_type = 'Lock'
                       AND state = 'active'
                       AND query LIKE '%UPDATE auto_queue_slots%'
                       AND query LIKE $1
                 )",
            )
            .bind(format!("%{query_fragment}%"))
            .fetch_one(&mut *conn)
            .await
            .expect("inspect blocked slot update");
            if blocked {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "allocator did not block on expected slot update"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn install_active_dispatch_after_slot_update_trigger(
        pool: &PgPool,
        function_name: &str,
        trigger_name: &str,
        dispatch_id: &str,
    ) {
        let function_sql = format!(
            "CREATE OR REPLACE FUNCTION {function_name}()
             RETURNS TRIGGER AS $$
             BEGIN
                 IF NEW.agent_id = 'agent-1'
                    AND NEW.slot_index = 0
                    AND NEW.assigned_run_id = 'run-1'
                    AND NEW.assigned_thread_group = 1 THEN
                     INSERT INTO task_dispatches (id, to_agent_id, status, context)
                     VALUES (
                         '{dispatch_id}',
                         'agent-1',
                         'dispatched',
                         jsonb_build_object('auto_queue', TRUE, 'slot_index', 0)::TEXT
                     )
                     ON CONFLICT (id) DO NOTHING;
                 END IF;
                 RETURN NEW;
             END;
             $$ LANGUAGE plpgsql"
        );
        sqlx::query(&function_sql)
            .execute(pool)
            .await
            .expect("create active dispatch trigger function");

        let trigger_sql = format!(
            "CREATE TRIGGER {trigger_name}
             AFTER UPDATE ON auto_queue_slots
             FOR EACH ROW EXECUTE FUNCTION {function_name}()"
        );
        sqlx::query(&trigger_sql)
            .execute(pool)
            .await
            .expect("create active dispatch trigger");
    }

    async fn count_transitions(pool: &PgPool, entry_id: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_entry_transitions
             WHERE entry_id = $1",
        )
        .bind(entry_id)
        .fetch_one(pool)
        .await
        .expect("transition count")
    }

    async fn count_message_outbox(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM message_outbox")
            .fetch_one(pool)
            .await
            .expect("message outbox count")
    }

    async fn seed_phase_gate_dispatches(pool: &PgPool, dispatch_ids: &[&str]) {
        for dispatch_id in dispatch_ids {
            sqlx::query(
                "INSERT INTO task_dispatches (id, to_agent_id, status, context)
                 VALUES ($1, 'agent-1', 'dispatched', '{}')",
            )
            .bind(dispatch_id)
            .execute(pool)
            .await
            .expect("seed phase gate dispatch");
        }
    }

    async fn phase_gate_row_ids(pool: &PgPool, phase: i64) -> Vec<i64> {
        sqlx::query(
            "SELECT id
             FROM auto_queue_phase_gates
             WHERE run_id = 'run-1' AND phase = $1
             ORDER BY COALESCE(dispatch_id, '') ASC",
        )
        .bind(phase)
        .fetch_all(pool)
        .await
        .expect("phase gate row ids")
        .into_iter()
        .map(|row| row.try_get::<i64, _>("id").expect("phase gate id"))
        .collect()
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_skips_reusable_slot_with_active_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET max_concurrent_threads = 2 WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("expand slot pool");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, slot_index, thread_group,
                 batch_phase, completed_at)
             VALUES ('entry-complete', 'run-1', NULL, 'agent-1', 'done', 0, 0, 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed completed slot entry");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-review-slot-0', 'agent-1', 'dispatched', $1)",
        )
        .bind(serde_json::json!({"slot_index": 0}).to_string())
        .execute(&pool)
        .await
        .expect("seed active dispatch in reusable slot");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-next', 'run-1', NULL, 'agent-1', 'pending', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("seed next phase entry");

        let allocation = allocate_slot_for_group_agent_pg(&pool, "run-1", 1, "agent-1")
            .await
            .expect("allocation must succeed via a different free slot");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 1,
                newly_assigned: true,
                reassigned_from_other_group: false,
            })
        );

        let slot_index: Option<i64> =
            sqlx::query_scalar("SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-next'")
                .fetch_one(&pool)
                .await
                .expect("next entry slot");
        assert_eq!(slot_index, Some(1));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_skips_rebind_update_with_active_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET max_concurrent_threads = 2 WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("expand slot pool");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, slot_index, thread_group,
                 batch_phase, completed_at)
             VALUES ('entry-rebind-complete', 'run-1', NULL, 'agent-1', 'done', 0, 0, 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed completed slot entry");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-rebind-next', 'run-1', NULL, 'agent-1', 'pending', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("seed next phase entry");

        let mut lock_conn = lock_slot_row_on_conn(&pg_db.database_url, "agent-1", 0).await;

        let mut seed_conn = PgConnection::connect(&pg_db.database_url)
            .await
            .expect("connect race seed connection");
        let pool_for_allocation = pool.clone();
        let allocation_task = tokio::spawn(async move {
            allocate_slot_for_group_agent_pg(&pool_for_allocation, "run-1", 1, "agent-1").await
        });
        wait_for_blocked_slot_update(&mut seed_conn, "SET assigned_thread_group").await;
        seed_active_slot_dispatch_on_conn(&mut seed_conn, "dispatch-rebind-race-slot-0", 0).await;
        sqlx::query("COMMIT")
            .execute(&mut lock_conn)
            .await
            .expect("release slot lock");
        lock_conn.close().await.expect("close slot lock connection");
        seed_conn.close().await.expect("close race seed connection");

        let allocation = allocation_task
            .await
            .expect("allocation task join")
            .expect("allocation must succeed via a different free slot");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 1,
                newly_assigned: true,
                reassigned_from_other_group: false,
            })
        );
        assert_eq!(slot_group(&pool, "agent-1", 0).await, Some(0));

        let slot_index: Option<i64> = sqlx::query_scalar(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-rebind-next'",
        )
        .fetch_one(&pool)
        .await
        .expect("next entry slot");
        assert_eq!(slot_index, Some(1));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_restores_rebind_when_dispatch_appears_after_update() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET max_concurrent_threads = 2 WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("expand slot pool");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, slot_index, thread_group,
                 batch_phase, completed_at)
             VALUES ('entry-rebind-post-complete', 'run-1', NULL, 'agent-1', 'done', 0, 0, 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed completed slot entry");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-rebind-post-next', 'run-1', NULL, 'agent-1', 'pending', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("seed next phase entry");
        install_active_dispatch_after_slot_update_trigger(
            &pool,
            "test_seed_rebind_post_update_dispatch",
            "test_seed_rebind_post_update_dispatch_trigger",
            "dispatch-rebind-post-update-slot-0",
        )
        .await;

        let allocation = allocate_slot_for_group_agent_pg(&pool, "run-1", 1, "agent-1")
            .await
            .expect("allocation must succeed via a different free slot");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 1,
                newly_assigned: true,
                reassigned_from_other_group: false,
            })
        );
        assert_eq!(slot_group(&pool, "agent-1", 0).await, Some(0));

        let slot_index: Option<i64> = sqlx::query_scalar(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-rebind-post-next'",
        )
        .fetch_one(&pool)
        .await
        .expect("next entry slot");
        assert_eq!(slot_index, Some(1));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_skips_free_slot_fallback_with_active_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET max_concurrent_threads = 2 WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("expand slot pool");
        sqlx::query(
            "UPDATE auto_queue_slots
             SET assigned_run_id = NULL,
                 assigned_thread_group = NULL
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .execute(&pool)
        .await
        .expect("free seed slot");
        seed_active_slot_dispatch(&pool, "dispatch-free-select-slot-0", 0).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-free-select-next', 'run-1', NULL, 'agent-1', 'pending', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("seed next phase entry");

        let allocation = allocate_slot_for_group_agent_pg(&pool, "run-1", 1, "agent-1")
            .await
            .expect("allocation must succeed via a different free slot");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 1,
                newly_assigned: true,
                reassigned_from_other_group: false,
            })
        );
        assert_eq!(slot_run(&pool, "agent-1", 0).await, None);

        let slot_index: Option<i64> = sqlx::query_scalar(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-free-select-next'",
        )
        .fetch_one(&pool)
        .await
        .expect("next entry slot");
        assert_eq!(slot_index, Some(1));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_releases_claim_when_dispatch_appears_after_update() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET max_concurrent_threads = 2 WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("expand slot pool");
        sqlx::query(
            "UPDATE auto_queue_slots
             SET assigned_run_id = NULL,
                 assigned_thread_group = NULL
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .execute(&pool)
        .await
        .expect("free seed slot");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-free-post-claim-next', 'run-1', NULL, 'agent-1', 'pending', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("seed next phase entry");
        install_active_dispatch_after_slot_update_trigger(
            &pool,
            "test_seed_free_post_update_dispatch",
            "test_seed_free_post_update_dispatch_trigger",
            "dispatch-free-post-update-slot-0",
        )
        .await;

        let allocation = allocate_slot_for_group_agent_pg(&pool, "run-1", 1, "agent-1")
            .await
            .expect("allocation must succeed via a different free slot");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 1,
                newly_assigned: true,
                reassigned_from_other_group: false,
            })
        );
        assert_eq!(slot_run(&pool, "agent-1", 0).await, None);

        let slot_index: Option<i64> = sqlx::query_scalar(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-free-post-claim-next'",
        )
        .fetch_one(&pool)
        .await
        .expect("next entry slot");
        assert_eq!(slot_index, Some(1));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_skips_free_slot_claim_with_active_dispatch() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET max_concurrent_threads = 2 WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("expand slot pool");
        sqlx::query(
            "UPDATE auto_queue_slots
             SET assigned_run_id = NULL,
                 assigned_thread_group = NULL
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .execute(&pool)
        .await
        .expect("free seed slot");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-free-claim-next', 'run-1', NULL, 'agent-1', 'pending', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("seed next phase entry");

        let mut lock_conn = lock_slot_row_on_conn(&pg_db.database_url, "agent-1", 0).await;

        let mut seed_conn = PgConnection::connect(&pg_db.database_url)
            .await
            .expect("connect race seed connection");
        let pool_for_allocation = pool.clone();
        let allocation_task = tokio::spawn(async move {
            allocate_slot_for_group_agent_pg(&pool_for_allocation, "run-1", 1, "agent-1").await
        });
        wait_for_blocked_slot_update(&mut seed_conn, "SET assigned_run_id").await;
        seed_active_slot_dispatch_on_conn(&mut seed_conn, "dispatch-free-claim-race-slot-0", 0)
            .await;
        sqlx::query("COMMIT")
            .execute(&mut lock_conn)
            .await
            .expect("release slot lock");
        lock_conn.close().await.expect("close slot lock connection");
        seed_conn.close().await.expect("close race seed connection");

        let allocation = allocation_task
            .await
            .expect("allocation task join")
            .expect("allocation must succeed via a different free slot");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 1,
                newly_assigned: true,
                reassigned_from_other_group: false,
            })
        );
        assert_eq!(slot_run(&pool, "agent-1", 0).await, None);

        let slot_index: Option<i64> = sqlx::query_scalar(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-free-claim-next'",
        )
        .fetch_one(&pool)
        .await
        .expect("next entry slot");
        assert_eq!(slot_index, Some(1));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn pending_review_dispatch_without_session_blocks_slot_reuse_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, dispatch_type, status, context)
             VALUES ('dispatch-pending-review-slot', 'agent-1', 'review', 'pending', $1)",
        )
        .bind(serde_json::json!({"slot_index": 0}).to_string())
        .execute(&pool)
        .await
        .expect("seed pending review dispatch");

        assert!(
            slot_has_active_dispatch_pg(&pool, "agent-1", 0)
                .await
                .expect("query pending review dispatch"),
            "pending review dispatches must occupy the slot before a provider session attaches"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn pending_review_dispatch_without_session_blocks_slot_reset_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, dispatch_type, status, context)
             VALUES ('dispatch-pending-review-reset', 'agent-1', 'review', 'pending', $1)",
        )
        .bind(serde_json::json!({"slot_index": 0}).to_string())
        .execute(&pool)
        .await
        .expect("seed pending review dispatch");

        let err = crate::services::auto_queue::runtime::reset_slot_thread_bindings_excluding_pg(
            &pool, "agent-1", 0, None, None,
        )
        .await
        .expect_err("pending review dispatch must block slot reset before session attach");
        assert!(err.contains("has active dispatch"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_rejects_pending_review_slot_blocker() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "UPDATE auto_queue_slots
             SET assigned_run_id = NULL,
                 assigned_thread_group = NULL
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .execute(&pool)
        .await
        .expect("free seed slot");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, context)
             VALUES ('dispatch-pending-review-claim', NULL, 'agent-1', 'review', 'pending', $1)",
        )
        .bind(serde_json::json!({"slot_index": 0}).to_string())
        .execute(&pool)
        .await
        .expect("seed pending review slot dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-after-pending-review', 'run-1', NULL, 'agent-1', 'pending', 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed pending entry");

        let allocation = allocate_slot_for_group_agent_pg(&pool, "run-1", 0, "agent-1")
            .await
            .expect("allocation probe should succeed");
        assert_eq!(
            allocation, None,
            "a pending review dispatch without a session must keep its slot busy"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn phase_gate_state_save_is_idempotent_for_dispatch_rows_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        seed_phase_gate_dispatches(&pool, &["dispatch-gate-1", "dispatch-gate-2"]).await;
        let state = PhaseGateStateWrite {
            status: "pending".to_string(),
            verdict: None,
            dispatch_ids: vec!["dispatch-gate-1".to_string(), "dispatch-gate-2".to_string()],
            pass_verdict: "phase_gate_passed".to_string(),
            next_phase: Some(6),
            final_phase: false,
            anchor_card_id: None,
            failure_reason: None,
            created_at: Some("2026-05-05 00:00:00+00".to_string()),
        };

        let first = save_phase_gate_state_on_pg(&pool, "run-1", 5, &state)
            .await
            .expect("first save phase gate state");
        let first_row_ids = phase_gate_row_ids(&pool, 5).await;
        let second = save_phase_gate_state_on_pg(&pool, "run-1", 5, &state)
            .await
            .expect("second save phase gate state");
        let second_row_ids = phase_gate_row_ids(&pool, 5).await;

        assert_eq!(
            first.persisted_dispatch_ids,
            vec!["dispatch-gate-1".to_string(), "dispatch-gate-2".to_string()]
        );
        assert_eq!(first.removed_stale_rows, 0);
        assert_eq!(second, first);
        assert_eq!(first_row_ids.len(), 2);
        assert_eq!(second_row_ids, first_row_ids);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn phase_gate_state_save_is_idempotent_for_empty_dispatch_set_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        let state = PhaseGateStateWrite {
            status: "pending".to_string(),
            verdict: None,
            dispatch_ids: Vec::new(),
            pass_verdict: "phase_gate_passed".to_string(),
            next_phase: None,
            final_phase: true,
            anchor_card_id: None,
            failure_reason: None,
            created_at: Some("2026-05-05 00:00:00+00".to_string()),
        };

        let first = save_phase_gate_state_on_pg(&pool, "run-1", 6, &state)
            .await
            .expect("first save empty phase gate state");
        let first_row_ids = phase_gate_row_ids(&pool, 6).await;
        let second = save_phase_gate_state_on_pg(&pool, "run-1", 6, &state)
            .await
            .expect("second save empty phase gate state");
        let second_row_ids = phase_gate_row_ids(&pool, 6).await;

        assert_eq!(first.persisted_dispatch_ids, Vec::<String>::new());
        assert_eq!(first.removed_stale_rows, 0);
        assert_eq!(second, first);
        assert_eq!(first_row_ids.len(), 1);
        assert_eq!(second_row_ids, first_row_ids);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn phase_gate_state_save_rolls_back_stale_cleanup_when_write_fails_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        seed_phase_gate_dispatches(&pool, &["dispatch-valid", "dispatch-stale"]).await;
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates
                (run_id, phase, status, dispatch_id, pass_verdict)
             VALUES ('run-1', 7, 'pending', 'dispatch-stale', 'phase_gate_passed')",
        )
        .execute(&pool)
        .await
        .expect("seed stale phase gate row");

        let error = save_phase_gate_state_on_pg(
            &pool,
            "run-1",
            7,
            &PhaseGateStateWrite {
                status: "pending".to_string(),
                verdict: None,
                dispatch_ids: vec!["dispatch-valid".to_string()],
                pass_verdict: "phase_gate_passed".to_string(),
                next_phase: None,
                final_phase: false,
                anchor_card_id: None,
                failure_reason: None,
                created_at: Some("not-a-timestamp".to_string()),
            },
        )
        .await
        .expect_err("invalid timestamp must fail the write");
        assert!(
            error.contains("upsert postgres phase-gate row"),
            "unexpected error: {error}"
        );

        let stale_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_phase_gates
             WHERE run_id = 'run-1' AND phase = 7 AND dispatch_id = 'dispatch-stale'",
        )
        .fetch_one(&pool)
        .await
        .expect("stale count");
        let valid_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_phase_gates
             WHERE run_id = 'run-1' AND phase = 7 AND dispatch_id = 'dispatch-valid'",
        )
        .fetch_one(&pool)
        .await
        .expect("valid count");
        assert_eq!(stale_count, 1);
        assert_eq!(valid_count, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn phase_gate_state_concurrent_clear_waits_for_atomic_save_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        seed_phase_gate_dispatches(&pool, &["dispatch-slow-1", "dispatch-slow-2"]).await;
        sqlx::query(
            r#"
            CREATE OR REPLACE FUNCTION slow_phase_gate_insert_for_test()
            RETURNS trigger AS $$
            BEGIN
                PERFORM pg_sleep(0.08);
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .execute(&pool)
        .await
        .expect("install slow phase gate insert function");
        sqlx::query(
            "CREATE TRIGGER slow_phase_gate_insert_for_test
             BEFORE INSERT ON auto_queue_phase_gates
             FOR EACH ROW EXECUTE FUNCTION slow_phase_gate_insert_for_test()",
        )
        .execute(&pool)
        .await
        .expect("install slow phase gate insert trigger");

        let pool_for_save = pool.clone();
        let save_state = PhaseGateStateWrite {
            status: "pending".to_string(),
            verdict: None,
            dispatch_ids: vec!["dispatch-slow-1".to_string(), "dispatch-slow-2".to_string()],
            pass_verdict: "phase_gate_passed".to_string(),
            next_phase: Some(10),
            final_phase: false,
            anchor_card_id: None,
            failure_reason: None,
            created_at: Some("2026-05-05 00:00:00+00".to_string()),
        };
        let save_task = tokio::spawn(async move {
            save_phase_gate_state_on_pg(&pool_for_save, "run-1", 9, &save_state).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let pool_for_clear = pool.clone();
        let clear_task =
            tokio::spawn(
                async move { clear_phase_gate_state_on_pg(&pool_for_clear, "run-1", 9).await },
            );

        let save_result = save_task
            .await
            .expect("save task join")
            .expect("save phase gate state");
        let cleared = clear_task
            .await
            .expect("clear task join")
            .expect("clear phase gate state");
        assert_eq!(
            save_result.persisted_dispatch_ids,
            vec!["dispatch-slow-1".to_string(), "dispatch-slow-2".to_string()]
        );
        assert!(cleared);
        let remaining = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_phase_gates
             WHERE run_id = 'run-1' AND phase = 9",
        )
        .fetch_one(&pool)
        .await
        .expect("remaining phase gate count");
        assert_eq!(remaining, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn slot_has_recent_terminal_auto_queue_dispatch_pg_respects_cooldown() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context, created_at, updated_at, completed_at)
             VALUES ('dispatch-recent-terminal-slot-0', 'agent-1', 'completed', $1, NOW(), NOW(), NOW())",
        )
        .bind(
            serde_json::json!({
                "auto_queue": true,
                "entry_id": "entry-recent-terminal-slot-0",
                "thread_group": 0,
                "slot_index": 0
            })
            .to_string(),
        )
        .execute(&pool)
        .await
        .expect("seed recent terminal dispatch");

        assert!(
            slot_has_recent_terminal_auto_queue_dispatch_pg(&pool, "agent-1", 0)
                .await
                .expect("recent terminal cooldown probe"),
            "recent same-slot terminal auto-queue dispatch must trigger cooldown"
        );
        assert!(
            !slot_has_recent_terminal_auto_queue_dispatch_pg(&pool, "agent-1", 1)
                .await
                .expect("other slot cooldown probe"),
            "dispatches in other slots must not trigger cooldown"
        );

        sqlx::query(
            "UPDATE task_dispatches
             SET completed_at = NOW() - INTERVAL '2 minutes',
                 updated_at = NOW() - INTERVAL '2 minutes'
             WHERE id = 'dispatch-recent-terminal-slot-0'",
        )
        .execute(&pool)
        .await
        .expect("age terminal dispatch");
        assert!(
            !slot_has_recent_terminal_auto_queue_dispatch_pg(&pool, "agent-1", 0)
                .await
                .expect("aged terminal cooldown probe"),
            "aged terminal dispatches should be eligible for the next tick"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_does_not_reuse_busy_existing_group_slot() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group,
                 batch_phase)
             VALUES ('entry-active', 'run-1', NULL, 'agent-1', 'dispatched', 'dispatch-active', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed active same-group entry");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-active', 'agent-1', 'pending', $1)",
        )
        .bind(
            serde_json::json!({
                "auto_queue": true,
                "entry_id": "entry-active",
                "slot_index": 0,
                "thread_group": 0
            })
            .to_string(),
        )
        .execute(&pool)
        .await
        .expect("seed pending same-slot dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-next', 'run-1', NULL, 'agent-1', 'pending', 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed next same-group entry");

        let allocation = allocate_slot_for_group_agent_pg(&pool, "run-1", 0, "agent-1")
            .await
            .expect("busy same-group slot probe must succeed");
        assert_eq!(
            allocation, None,
            "a group must not receive its existing slot while that slot has a pending dispatch"
        );

        let next_slot: Option<i64> =
            sqlx::query_scalar("SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-next'")
                .fetch_one(&pool)
                .await
                .expect("next entry slot");
        assert_eq!(next_slot, None);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn dispatch_terminal_sync_marks_entry_done_without_finalizing_active_run_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ('card-sync-done', 'Card Sync Done', 'in_progress', 'agent-1')",
        )
        .execute(&pool)
        .await
        .expect("seed card");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, context)
             VALUES ('dispatch-sync-done', 'card-sync-done', 'agent-1',
                     'implementation', 'completed', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-sync-done', 'run-1', 'card-sync-done', 'agent-1',
                     'dispatched', 'dispatch-sync-done', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let mut tx = pool.begin().await.expect("begin tx");
        let changed = sync_dispatch_terminal_entries_on_pg_tx(
            &mut tx,
            "dispatch-sync-done",
            ENTRY_STATUS_DONE,
            "test_runtime_finalizer",
            true,
        )
        .await
        .expect("sync dispatch terminal");
        tx.commit().await.expect("commit tx");

        assert_eq!(changed, 1);
        let (status, dispatch_id, completed_at) =
            entry_row_status_dispatch_completed(&pool, "entry-sync-done").await;
        assert_eq!(status, ENTRY_STATUS_DONE);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-sync-done"));
        assert!(completed_at.is_some());
        assert_eq!(run_status(&pool, "run-1").await, "active");
        assert_eq!(
            slot_run(&pool, "agent-1", 0).await.as_deref(),
            Some("run-1")
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn dispatch_terminal_sync_does_not_restore_stale_dispatch_link_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ('card-sync-race', 'Card Sync Race', 'in_progress', 'agent-1')",
        )
        .execute(&pool)
        .await
        .expect("seed card");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, context)
             VALUES
                ('dispatch-sync-old', 'card-sync-race', 'agent-1',
                 'implementation', 'failed', '{}'),
                ('dispatch-sync-new', 'card-sync-race', 'agent-1',
                 'implementation', 'pending', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatches");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-sync-race', 'run-1', 'card-sync-race', 'agent-1',
                     'dispatched', 'dispatch-sync-old', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");
        sqlx::query(
            "CREATE OR REPLACE FUNCTION test_reassign_entry_during_terminal_sync()
             RETURNS TRIGGER AS $$
             BEGIN
                 IF NEW.id = 'entry-sync-race'
                    AND NEW.status = 'failed'
                    AND NEW.dispatch_id IS NULL THEN
                     UPDATE auto_queue_entries
                     SET dispatch_id = 'dispatch-sync-new',
                         slot_index = 1
                     WHERE id = NEW.id;
                 END IF;
                 RETURN NEW;
             END;
             $$ LANGUAGE plpgsql",
        )
        .execute(&pool)
        .await
        .expect("create reassign trigger function");
        sqlx::query(
            "CREATE TRIGGER test_reassign_entry_during_terminal_sync_trigger
             AFTER UPDATE ON auto_queue_entries
             FOR EACH ROW EXECUTE FUNCTION test_reassign_entry_during_terminal_sync()",
        )
        .execute(&pool)
        .await
        .expect("create reassign trigger");

        let mut tx = pool.begin().await.expect("begin tx");
        let changed = sync_dispatch_terminal_entries_on_pg_tx(
            &mut tx,
            "dispatch-sync-old",
            ENTRY_STATUS_FAILED,
            "test_runtime_finalizer",
            true,
        )
        .await
        .expect("sync dispatch terminal");
        tx.commit().await.expect("commit tx");

        assert_eq!(changed, 1);
        let row = sqlx::query(
            "SELECT status, dispatch_id, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-sync-race'",
        )
        .fetch_one(&pool)
        .await
        .expect("entry row");
        assert_eq!(
            row.try_get::<String, _>("status").unwrap(),
            ENTRY_STATUS_FAILED
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("dispatch_id")
                .unwrap()
                .as_deref(),
            Some("dispatch-sync-new")
        );
        assert_eq!(
            row.try_get::<Option<i64>, _>("slot_index").unwrap(),
            Some(1)
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn completed_retry_reconciles_failed_entry_for_same_card_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ('card-retry-done', 'Card Retry Done', 'done', 'agent-1')",
        )
        .execute(&pool)
        .await
        .expect("seed card");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, context, completed_at)
             VALUES
                ('dispatch-retry-old', 'card-retry-done', 'agent-1',
                 'implementation', 'cancelled', '{}', NOW()),
                ('dispatch-retry-new', 'card-retry-done', 'agent-1',
                 'implementation', 'completed', '{}', NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed dispatches");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 retry_count, thread_group, batch_phase, completed_at)
             VALUES ('entry-retry-done', 'run-1', 'card-retry-done', 'agent-1',
                     'failed', 'dispatch-retry-old', 0, 1, 0, 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed failed entry");

        let mut tx = pool.begin().await.expect("begin tx");
        let result = finalize_completed_dispatch_terminal_entry_on_pg_tx(
            &mut tx,
            "dispatch-retry-new",
            "watcher_streaming_final",
            true,
        )
        .await
        .expect("finalize completed retry dispatch entry");
        tx.commit().await.expect("commit tx");

        assert_eq!(result.changed_entries, 1);
        assert_eq!(result.affected_run_ids, vec!["run-1".to_string()]);
        let (status, dispatch_id, completed_at) =
            entry_row_status_dispatch_completed(&pool, "entry-retry-done").await;
        assert_eq!(status, ENTRY_STATUS_DONE);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-retry-old"));
        assert!(completed_at.is_some());
        assert_eq!(count_transitions(&pool, "entry-retry-done").await, 1);
        let retry_history_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_entry_dispatch_history
             WHERE entry_id = 'entry-retry-done'
               AND dispatch_id = 'dispatch-retry-new'",
        )
        .fetch_one(&pool)
        .await
        .expect("retry dispatch history count");
        assert_eq!(retry_history_count, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn completed_dispatch_terminal_finalizer_completes_review_disabled_run_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET review_mode = 'disabled' WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("disable review mode");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ('card-finalizer-done', 'Card Finalizer Done', 'in_progress', 'agent-1')",
        )
        .execute(&pool)
        .await
        .expect("seed card");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, context)
             VALUES ('dispatch-finalizer-done', 'card-finalizer-done', 'agent-1',
                     'implementation', 'completed', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-finalizer-done', 'run-1', 'card-finalizer-done', 'agent-1',
                     'dispatched', 'dispatch-finalizer-done', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let mut tx = pool.begin().await.expect("begin tx");
        let result = finalize_completed_dispatch_terminal_entry_on_pg_tx(
            &mut tx,
            "dispatch-finalizer-done",
            "watcher_streaming_final",
            true,
        )
        .await
        .expect("finalize completed dispatch entry");
        tx.commit().await.expect("commit tx");

        assert_eq!(result.changed_entries, 1);
        assert_eq!(result.affected_run_ids, vec!["run-1".to_string()]);
        assert_eq!(result.finalized_run_ids, vec!["run-1".to_string()]);
        assert_eq!(
            entry_row_status_dispatch_completed(&pool, "entry-finalizer-done")
                .await
                .0,
            ENTRY_STATUS_DONE
        );
        assert_eq!(run_status(&pool, "run-1").await, "completed");
        assert_eq!(slot_run(&pool, "agent-1", 0).await, None);
        assert_eq!(count_transitions(&pool, "entry-finalizer-done").await, 1);
        assert_eq!(count_message_outbox(&pool).await, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn completed_dispatch_terminal_finalizer_is_idempotent_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET review_mode = 'disabled' WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("disable review mode");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ('card-finalizer-repeat', 'Card Finalizer Repeat', 'in_progress', 'agent-1')",
        )
        .execute(&pool)
        .await
        .expect("seed card");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, context)
             VALUES ('dispatch-finalizer-repeat', 'card-finalizer-repeat', 'agent-1',
                     'implementation', 'completed', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-finalizer-repeat', 'run-1', 'card-finalizer-repeat', 'agent-1',
                     'dispatched', 'dispatch-finalizer-repeat', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let mut tx = pool.begin().await.expect("begin first tx");
        let first = finalize_completed_dispatch_terminal_entry_on_pg_tx(
            &mut tx,
            "dispatch-finalizer-repeat",
            "watcher_streaming_final",
            true,
        )
        .await
        .expect("first finalize");
        tx.commit().await.expect("commit first tx");
        assert_eq!(first.changed_entries, 1);
        assert_eq!(first.finalized_run_ids, vec!["run-1".to_string()]);

        let transition_count = count_transitions(&pool, "entry-finalizer-repeat").await;
        let outbox_count = count_message_outbox(&pool).await;

        let mut tx = pool.begin().await.expect("begin second tx");
        let second = finalize_completed_dispatch_terminal_entry_on_pg_tx(
            &mut tx,
            "dispatch-finalizer-repeat",
            "watcher_streaming_final",
            true,
        )
        .await
        .expect("second finalize");
        tx.commit().await.expect("commit second tx");

        assert_eq!(second.changed_entries, 0);
        assert!(second.affected_run_ids.is_empty());
        assert!(second.finalized_run_ids.is_empty());
        assert_eq!(
            count_transitions(&pool, "entry-finalizer-repeat").await,
            transition_count
        );
        assert_eq!(count_message_outbox(&pool).await, outbox_count);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn user_cancelled_entry_does_not_finalize_review_disabled_run_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET review_mode = 'disabled' WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("disable review mode");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-user-cancelled', 'run-1', NULL, 'agent-1',
                     'dispatched', NULL, 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let result = update_entry_status_on_pg(
            &pool,
            "entry-user-cancelled",
            ENTRY_STATUS_USER_CANCELLED,
            "dispatch_cancel_user",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("user cancel entry");

        assert!(result.changed);
        assert_eq!(result.to_status, ENTRY_STATUS_USER_CANCELLED);
        assert_eq!(run_status(&pool, "run-1").await, "active");
        assert_eq!(
            slot_run(&pool, "agent-1", 0).await.as_deref(),
            Some("run-1")
        );
        assert_eq!(count_message_outbox(&pool).await, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn dispatch_terminal_sync_respects_blocking_phase_gate_on_paused_run_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query("UPDATE auto_queue_runs SET status = 'paused' WHERE id = 'run-1'")
            .execute(&pool)
            .await
            .expect("pause run");
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ('card-sync-skip', 'Card Sync Skip', 'in_progress', 'agent-1')",
        )
        .execute(&pool)
        .await
        .expect("seed card");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, context)
             VALUES ('dispatch-sync-skip', 'card-sync-skip', 'agent-1',
                     'implementation', 'cancelled', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-sync-skip', 'run-1', 'card-sync-skip', 'agent-1',
                     'dispatched', 'dispatch-sync-skip', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates
                (run_id, phase, status, verdict, pass_verdict, next_phase,
                 final_phase, anchor_card_id)
             VALUES ('run-1', 0, 'pending', NULL, 'phase_gate_passed',
                     NULL, TRUE, 'card-sync-skip')",
        )
        .execute(&pool)
        .await
        .expect("seed blocking phase gate");

        let mut tx = pool.begin().await.expect("begin tx");
        let changed = sync_dispatch_terminal_entries_on_pg_tx(
            &mut tx,
            "dispatch-sync-skip",
            ENTRY_STATUS_SKIPPED,
            "test_phase_gate_finalizer",
            true,
        )
        .await
        .expect("sync dispatch terminal");
        tx.commit().await.expect("commit tx");

        assert_eq!(changed, 1);
        assert_eq!(
            entry_row_status_dispatch_completed(&pool, "entry-sync-skip")
                .await
                .0,
            ENTRY_STATUS_SKIPPED
        );
        assert_eq!(run_status(&pool, "run-1").await, "paused");

        pool.close().await;
        pg_db.drop().await;
    }
}
