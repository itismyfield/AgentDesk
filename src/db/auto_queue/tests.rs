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
        ENTRY_STATUS_DONE, ENTRY_STATUS_SKIPPED, ENTRY_STATUS_USER_CANCELLED,
        EntryStatusUpdateOptions, PhaseGateStateWrite, SlotAllocation,
        allocate_slot_for_group_agent_pg, clear_phase_gate_state_on_pg,
        finalize_completed_dispatch_terminal_entry_on_pg_tx, save_phase_gate_state_on_pg,
        slot_has_recent_terminal_auto_queue_dispatch_pg, sync_dispatch_terminal_entries_on_pg_tx,
        update_entry_status_on_pg,
    };
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use chrono::{DateTime, Utc};
    use sqlx::{Connection, PgConnection, PgPool, Row};

    async fn setup_pool(pg_db: &TestPostgresDb) -> PgPool {
        let pool = pg_db.connect_and_migrate().await;
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        ENTRY_STATUS_DISPATCHED, ENTRY_STATUS_DONE, ENTRY_STATUS_FAILED, ENTRY_STATUS_PENDING,
        ENTRY_STATUS_SKIPPED, EntryStatusUpdateOptions, PhaseGateStateWrite, SlotAllocation,
        allocate_slot_for_group_agent_pg, clear_phase_gate_state_on_pg,
        latest_entry_phase_codex_session_id_pg, list_entry_dispatch_history_pg,
        reactivate_done_entry_on_pg, reconcile_failed_entry_done_on_pg,
        record_consultation_dispatch_on_pg, release_run_slots_pg, release_slot_for_group_agent_pg,
        save_phase_gate_state_on_pg, slot_has_active_dispatch_pg, update_entry_status_on_pg,
    };
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};

    /// Seed the canonical `setup_conn` baseline against PG: one active run
    /// (`run-1` / `agent-1`), one agent row, and one slot row pre-bound to
    /// `run-1` group 0. Returns the freshly-migrated pool.
    async fn setup_pool(pg_db: &TestPostgresDb) -> PgPool {
        let pool = pg_db.connect_and_migrate().await;
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

    /// Seed the shared-slot harness against PG: one active run with
    /// `max_concurrent_threads = 1` and two `pending` entries in different
    /// thread groups. Used by the concurrency test for
    /// `allocate_slot_for_group_agent_pg`.
    async fn setup_shared_slot_pool(pg_db: &TestPostgresDb) -> PgPool {
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO auto_queue_runs
                (id, repo, agent_id, status, max_concurrent_threads)
             VALUES ('run-shared', 'repo-1', 'agent-1', 'active', 1)",
        )
        .execute(&pool)
        .await
        .expect("seed shared run");
        sqlx::query("INSERT INTO agents (id, name, discord_channel_id) VALUES ('agent-1', 'Agent 1', '999')")
            .execute(&pool)
            .await
            .expect("seed agent");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-shared-0', 'run-shared', NULL, 'agent-1', 'pending', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed shared entry 0");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-shared-1', 'run-shared', NULL, 'agent-1', 'pending', 1)",
        )
        .execute(&pool)
        .await
        .expect("seed shared entry 1");
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
        let row = sqlx::query("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .expect("run row");
        row.try_get::<String, _>("status").expect("status")
    }

    async fn slot_assignment(
        pool: &PgPool,
        agent_id: &str,
        slot_index: i64,
    ) -> (Option<String>, Option<i64>) {
        let row = sqlx::query(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = $1 AND slot_index = $2",
        )
        .bind(agent_id)
        .bind(slot_index)
        .fetch_one(pool)
        .await
        .expect("slot row");
        (
            row.try_get::<Option<String>, _>("assigned_run_id")
                .expect("assigned_run_id"),
            row.try_get::<Option<i64>, _>("assigned_thread_group")
                .expect("assigned_thread_group"),
        )
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

    async fn count_outbox(pool: &PgPool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM message_outbox")
            .fetch_one(pool)
            .await
            .expect("outbox count")
    }

    #[tokio::test]
    async fn entry_transition_done_defers_run_completion_until_policy_hook_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group)
             VALUES ('entry-1', 'run-1', NULL, 'agent-1', 'pending', NULL, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let dispatched = update_entry_status_on_pg(
            &pool,
            "entry-1",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-1".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("dispatch transition");
        assert_eq!(dispatched.from_status, ENTRY_STATUS_PENDING);
        assert_eq!(dispatched.to_status, ENTRY_STATUS_DISPATCHED);

        update_entry_status_on_pg(
            &pool,
            "entry-1",
            ENTRY_STATUS_DONE,
            "test_done",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("done transition");

        let (status, dispatch_id, completed_at) =
            entry_row_status_dispatch_completed(&pool, "entry-1").await;
        assert_eq!(status, ENTRY_STATUS_DONE);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-1"));
        assert!(completed_at.is_some());
        assert_eq!(run_status(&pool, "run-1").await, "active");
        let slot = slot_assignment(&pool, "agent-1", 0).await;
        assert_eq!(slot.0.as_deref(), Some("run-1"));
        assert_eq!(count_transitions(&pool, "entry-1").await, 2);
        assert_eq!(
            count_outbox(&pool).await,
            0,
            "done transition must wait for policy-side completion before notifying"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_done_keeps_slot_assignment_until_multi_phase_run_finishes_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, batch_phase)
             VALUES ('entry-phase-0', 'run-1', NULL, 'agent-1', 'pending', NULL, 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed phase 0 entry");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-phase-1', 'run-1', NULL, 'agent-1', 'pending', 1, 1)",
        )
        .execute(&pool)
        .await
        .expect("seed phase 1 entry");

        update_entry_status_on_pg(
            &pool,
            "entry-phase-0",
            ENTRY_STATUS_DISPATCHED,
            "test_phase_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-phase-0".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("dispatch phase 0 entry");
        update_entry_status_on_pg(
            &pool,
            "entry-phase-0",
            ENTRY_STATUS_DONE,
            "test_phase_done",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("complete phase 0 entry");

        assert_eq!(run_status(&pool, "run-1").await, "active");
        let slot = slot_assignment(&pool, "agent-1", 0).await;
        assert_eq!(slot.0.as_deref(), Some("run-1"));
        assert_eq!(slot.1, Some(0));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_done_is_idempotent_without_duplicate_side_effects_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group)
             VALUES ('entry-idempotent', 'run-1', NULL, 'agent-1', 'dispatched',
                     'dispatch-idempotent', 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let first = update_entry_status_on_pg(
            &pool,
            "entry-idempotent",
            ENTRY_STATUS_DONE,
            "test_done_first",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("first completion");
        assert!(first.changed);

        let transition_count_before = count_transitions(&pool, "entry-idempotent").await;
        let outbox_count_before = count_outbox(&pool).await;

        let second = update_entry_status_on_pg(
            &pool,
            "entry-idempotent",
            ENTRY_STATUS_DONE,
            "test_done_second",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("second completion");
        assert!(
            !second.changed,
            "repeated terminal completion must become a no-op"
        );

        assert_eq!(
            count_transitions(&pool, "entry-idempotent").await,
            transition_count_before,
            "repeated completion must not append duplicate transition audit rows"
        );
        assert_eq!(
            count_outbox(&pool).await,
            outbox_count_before,
            "repeated completion must not emit duplicate completion notifications"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_pending_clears_dispatch_binding_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index,
                 thread_group, completed_at)
             VALUES ('entry-2', 'run-1', NULL, 'agent-1', 'dispatched', 'dispatch-2', 0, 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        update_entry_status_on_pg(
            &pool,
            "entry-2",
            ENTRY_STATUS_PENDING,
            "test_reset",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("pending reset");

        let row = sqlx::query(
            "SELECT status, dispatch_id, slot_index, completed_at
             FROM auto_queue_entries
             WHERE id = 'entry-2'",
        )
        .fetch_one(&pool)
        .await
        .expect("entry row");
        let status: String = row.try_get("status").expect("status");
        let dispatch_id: Option<String> = row.try_get("dispatch_id").expect("dispatch_id");
        let slot_index: Option<i64> = row.try_get("slot_index").expect("slot_index");
        let completed_at: Option<DateTime<Utc>> =
            row.try_get("completed_at").expect("completed_at");
        assert_eq!(status, ENTRY_STATUS_PENDING);
        assert!(dispatch_id.is_none());
        assert!(slot_index.is_none());
        assert!(completed_at.is_none());

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_dispatch_history_preserves_previous_dispatch_ids_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        // The dispatch-history FK requires task_dispatches rows.
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-consult', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch consult");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-impl', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch impl");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-history', 'run-1', NULL, 'agent-1', 'pending', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        update_entry_status_on_pg(
            &pool,
            "entry-history",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch_initial",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-consult".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("initial dispatch");
        update_entry_status_on_pg(
            &pool,
            "entry-history",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch_resume",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-impl".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("resumed dispatch");

        let history = list_entry_dispatch_history_pg(&pool, "entry-history")
            .await
            .expect("history");
        assert_eq!(history, vec!["dispatch-consult", "dispatch-impl"]);

        let current_dispatch_id: Option<String> = sqlx::query_scalar(
            "SELECT dispatch_id FROM auto_queue_entries WHERE id = 'entry-history'",
        )
        .fetch_one(&pool)
        .await
        .expect("current dispatch");
        assert_eq!(current_dispatch_id.as_deref(), Some("dispatch-impl"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn latest_entry_phase_codex_session_id_uses_same_phase_history_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-resume', 'run-1', NULL, 'agent-1', 'pending', 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, dispatch_type, status, context)
             VALUES
                ('dispatch-turn', 'agent-1', 'implementation', 'failed', '{}'),
                ('dispatch-review', 'agent-1', 'review', 'failed', '{}'),
                ('dispatch-context', 'agent-1', 'implementation', 'failed',
                    '{\"auto_queue_retry_resume_session_id\":\"context-session\"}'),
                ('dispatch-live', 'agent-1', 'implementation', 'failed', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatches");
        sqlx::query(
            "INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id, trigger_source)
             VALUES
                ('entry-resume', 'dispatch-turn', 'test'),
                ('entry-resume', 'dispatch-review', 'test'),
                ('entry-resume', 'dispatch-context', 'test'),
                ('entry-resume', 'dispatch-live', 'test')",
        )
        .execute(&pool)
        .await
        .expect("seed history");
        sqlx::query(
            "INSERT INTO turns
                (turn_id, channel_id, provider, session_id, dispatch_id, started_at, finished_at)
             VALUES
                ('turn-1', '123', 'codex', 'turn-session', 'dispatch-turn', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed turn");
        sqlx::query(
            "INSERT INTO sessions
                (session_key, agent_id, provider, status, active_dispatch_id, claude_session_id)
             VALUES
                ('codex/test/live', 'agent-1', 'codex', 'turn_active',
                 'dispatch-live', 'live-session')",
        )
        .execute(&pool)
        .await
        .expect("seed live session");

        assert_eq!(
            latest_entry_phase_codex_session_id_pg(&pool, "entry-resume", "implementation")
                .await
                .expect("lookup session")
                .as_deref(),
            Some("live-session")
        );
        sqlx::query("DELETE FROM sessions WHERE active_dispatch_id = 'dispatch-live'")
            .execute(&pool)
            .await
            .expect("remove live session");
        assert_eq!(
            latest_entry_phase_codex_session_id_pg(&pool, "entry-resume", "implementation")
                .await
                .expect("lookup context fallback")
                .as_deref(),
            Some("context-session")
        );
        sqlx::query(
            "DELETE FROM auto_queue_entry_dispatch_history
             WHERE entry_id = 'entry-resume' AND dispatch_id = 'dispatch-context'",
        )
        .execute(&pool)
        .await
        .expect("remove context fallback");
        assert_eq!(
            latest_entry_phase_codex_session_id_pg(&pool, "entry-resume", "implementation")
                .await
                .expect("lookup turn fallback")
                .as_deref(),
            Some("turn-session")
        );
        assert_eq!(
            latest_entry_phase_codex_session_id_pg(&pool, "entry-resume", "review")
                .await
                .expect("lookup review session"),
            None,
            "review phase history must not leak into implementation retries"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// PG variant of the SQLite stale-current-row retry test. The PG twin
    /// `update_entry_status_on_pg` does not expose a
    /// `..._with_current_on_conn`-style entry point — instead it carries the
    /// retry loop internally, re-loading the row when the optimistic UPDATE
    /// matches zero rows. This test simulates concurrent dispatch by writing
    /// a `dispatched` row through the helper and then asking the helper to
    /// transition straight to `skipped`, exercising the same allowed
    /// `dispatched -> skipped` path the SQLite test ultimately verified.
    #[tokio::test]
    async fn stale_allowed_transition_retries_from_latest_status_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-live', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-stale', 'run-1', NULL, 'agent-1', 'pending', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        // Move the entry to dispatched first, then ask the shared helper to
        // skip — the PG helper resolves the latest status before retrying.
        update_entry_status_on_pg(
            &pool,
            "entry-stale",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-live".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("simulate concurrent dispatch");

        let result = update_entry_status_on_pg(
            &pool,
            "entry-stale",
            ENTRY_STATUS_SKIPPED,
            "test_cancel_retry",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("stale cancel should succeed");
        assert!(result.changed);
        assert_eq!(result.from_status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(result.to_status, ENTRY_STATUS_SKIPPED);

        let (status, dispatch_id, completed_at) =
            entry_row_status_dispatch_completed(&pool, "entry-stale").await;
        assert_eq!(status, ENTRY_STATUS_SKIPPED);
        assert!(dispatch_id.is_none());
        assert!(completed_at.is_some());

        let row = sqlx::query(
            "SELECT from_status, to_status
             FROM auto_queue_entry_transitions
             WHERE entry_id = 'entry-stale'
             ORDER BY id DESC
             LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("transition row");
        let from_status: String = row.try_get("from_status").expect("from_status");
        let to_status: String = row.try_get("to_status").expect("to_status");
        assert_eq!(from_status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(to_status, ENTRY_STATUS_SKIPPED);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_allows_skipped_restore_to_dispatched_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-restored', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-3', 'run-1', NULL, 'agent-1', 'skipped', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let restored = update_entry_status_on_pg(
            &pool,
            "entry-3",
            ENTRY_STATUS_DISPATCHED,
            "test_restore_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-restored".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("restore transition");
        assert!(restored.changed);
        assert_eq!(restored.from_status, ENTRY_STATUS_SKIPPED);
        assert_eq!(restored.to_status, ENTRY_STATUS_DISPATCHED);

        let row = sqlx::query(
            "SELECT status, dispatch_id, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-3'",
        )
        .fetch_one(&pool)
        .await
        .expect("entry row");
        let status: String = row.try_get("status").expect("status");
        let dispatch_id: Option<String> = row.try_get("dispatch_id").expect("dispatch_id");
        let slot_index: Option<i64> = row.try_get("slot_index").expect("slot_index");
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-restored"));
        assert_eq!(slot_index, Some(0));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_allows_done_restore_to_dispatched_for_recovery_sources_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-rereview', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, completed_at)
             VALUES ('entry-3b', 'run-1', NULL, 'agent-1', 'done', 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed done entry");

        let restored = update_entry_status_on_pg(
            &pool,
            "entry-3b",
            ENTRY_STATUS_DISPATCHED,
            "rereview_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-rereview".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("recovery transition");
        assert!(restored.changed);
        assert_eq!(restored.from_status, ENTRY_STATUS_DONE);
        assert_eq!(restored.to_status, ENTRY_STATUS_DISPATCHED);

        let row = sqlx::query(
            "SELECT status, dispatch_id, slot_index, completed_at
             FROM auto_queue_entries
             WHERE id = 'entry-3b'",
        )
        .fetch_one(&pool)
        .await
        .expect("entry row");
        let status: String = row.try_get("status").expect("status");
        let dispatch_id: Option<String> = row.try_get("dispatch_id").expect("dispatch_id");
        let slot_index: Option<i64> = row.try_get("slot_index").expect("slot_index");
        let completed_at: Option<DateTime<Utc>> =
            row.try_get("completed_at").expect("completed_at");
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-rereview"));
        assert_eq!(slot_index, Some(0));
        assert!(completed_at.is_none());

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_blocks_invalid_done_to_pending_restore_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-4', 'run-1', NULL, 'agent-1', 'done', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed done entry");

        let error = update_entry_status_on_pg(
            &pool,
            "entry-4",
            ENTRY_STATUS_PENDING,
            "test_invalid",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect_err("invalid transition must fail");
        assert!(
            error.contains("invalid auto-queue entry transition"),
            "expected invalid-transition error, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_blocks_invalid_done_to_dispatched_restore_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-retry', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-4b', 'run-1', NULL, 'agent-1', 'done', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed done entry");

        let error = update_entry_status_on_pg(
            &pool,
            "entry-4b",
            ENTRY_STATUS_DISPATCHED,
            "test_invalid_done_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-retry".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect_err("done -> dispatched transition must fail");
        assert!(
            error.contains("invalid auto-queue entry transition"),
            "expected invalid-transition error, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn entry_transition_blocks_invalid_done_to_skipped_restore_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-4c', 'run-1', NULL, 'agent-1', 'done', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed done entry");

        let error = update_entry_status_on_pg(
            &pool,
            "entry-4c",
            ENTRY_STATUS_SKIPPED,
            "test_invalid_done_skip",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect_err("done -> skipped transition must fail");
        assert!(
            error.contains("invalid auto-queue entry transition"),
            "expected invalid-transition error, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn reactivate_done_entry_allows_admin_restore_to_dispatched_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-reactivate', 'repo-1', 'agent-1', 'completed')",
        )
        .execute(&pool)
        .await
        .expect("seed run");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, completed_at)
             VALUES ('entry-reactivate', 'run-reactivate', NULL, 'agent-1', 'done', 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed done entry");

        let restored = reactivate_done_entry_on_pg(
            &pool,
            "entry-reactivate",
            "test_reactivate_done",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("reactivate done entry");
        assert!(restored.changed);
        assert_eq!(restored.from_status, ENTRY_STATUS_DONE);
        assert_eq!(restored.to_status, ENTRY_STATUS_DISPATCHED);

        let (status, dispatch_id, completed_at) =
            entry_row_status_dispatch_completed(&pool, "entry-reactivate").await;
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert!(dispatch_id.is_none());
        assert!(completed_at.is_none());

        assert_eq!(run_status(&pool, "run-reactivate").await, "active");

        pool.close().await;
        pg_db.drop().await;
    }

    /// Two concurrent allocations against a single-slot pool must succeed at
    /// most once. The PG twin's optimistic CAS retry loop handles concurrent
    /// claims; a second tokio task racing the same pool must observe either
    /// `None` (no slot available) or a successful allocation, but never both
    /// claims at the same time.
    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_never_double_assigns_single_slot_under_concurrency() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_shared_slot_pool(&pg_db).await;

        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let task_a = tokio::spawn(async move {
            allocate_slot_for_group_agent_pg(&pool_a, "run-shared", 0, "agent-1").await
        });
        let task_b = tokio::spawn(async move {
            allocate_slot_for_group_agent_pg(&pool_b, "run-shared", 1, "agent-1").await
        });
        let first = task_a.await.unwrap().expect("first allocation");
        let second = task_b.await.unwrap().expect("second allocation");

        let successful: Vec<SlotAllocation> = [first, second].into_iter().flatten().collect();
        assert_eq!(
            successful.len(),
            1,
            "single-slot pool must allow only one concurrent group allocation"
        );

        let assignments: Vec<(Option<String>, Option<i64>)> = sqlx::query(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-1'
             ORDER BY slot_index ASC",
        )
        .fetch_all(&pool)
        .await
        .expect("slot rows")
        .into_iter()
        .map(|row| {
            (
                row.try_get::<Option<String>, _>("assigned_run_id")
                    .expect("assigned_run_id"),
                row.try_get::<Option<i64>, _>("assigned_thread_group")
                    .expect("assigned_thread_group"),
            )
        })
        .collect();
        assert_eq!(assignments.len(), 1);
        assert_eq!(
            assignments[0].0.as_deref(),
            Some("run-shared"),
            "the slot must remain assigned to exactly one run"
        );

        let slotted_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_entries
             WHERE slot_index IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .expect("slotted count");
        assert_eq!(
            slotted_count, 1,
            "only one group entry must receive the single slot"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_rebinds_completed_same_run_slot_without_reset() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "UPDATE auto_queue_slots
             SET thread_id_map = CAST($1 AS jsonb)
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .bind(r#"{"123":"thread-slot-0"}"#)
        .execute(&pool)
        .await
        .expect("seed slot thread map");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, slot_index, thread_group,
                 batch_phase, completed_at)
             VALUES ('entry-complete', 'run-1', NULL, 'agent-1', 'done', 0, 0, 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed completed entry");
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
            .expect("same-run rebind must succeed");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 0,
                newly_assigned: false,
                reassigned_from_other_group: true,
            })
        );

        let row = sqlx::query(
            "SELECT assigned_run_id, assigned_thread_group, thread_id_map::TEXT AS thread_id_map
             FROM auto_queue_slots
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .fetch_one(&pool)
        .await
        .expect("slot row");
        let assigned_run_id: Option<String> =
            row.try_get("assigned_run_id").expect("assigned_run_id");
        let assigned_thread_group: Option<i64> = row
            .try_get("assigned_thread_group")
            .expect("assigned_thread_group");
        let thread_id_map: Option<String> = row.try_get("thread_id_map").expect("thread_id_map");
        assert_eq!(assigned_run_id.as_deref(), Some("run-1"));
        assert_eq!(assigned_thread_group, Some(1));
        let parsed: serde_json::Value =
            serde_json::from_str(thread_id_map.as_deref().unwrap_or("{}"))
                .expect("thread_id_map json");
        assert_eq!(parsed["123"], "thread-slot-0");

        let slot_index: Option<i64> =
            sqlx::query_scalar("SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-next'")
                .fetch_one(&pool)
                .await
                .expect("next entry slot");
        assert_eq!(slot_index, Some(0));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_marks_cross_run_reclaim_as_new_assignment() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "UPDATE auto_queue_slots
             SET thread_id_map = CAST($1 AS jsonb)
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .bind(r#"{"123":"thread-slot-0"}"#)
        .execute(&pool)
        .await
        .expect("seed slot thread map");
        release_run_slots_pg(&pool, "run-1")
            .await
            .expect("release first run slots");
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-2', 'repo-1', 'agent-1', 'active')",
        )
        .execute(&pool)
        .await
        .expect("seed second run");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-run-2', 'run-2', NULL, 'agent-1', 'pending', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed second run entry");

        let allocation = allocate_slot_for_group_agent_pg(&pool, "run-2", 0, "agent-1")
            .await
            .expect("cross-run claim must succeed");
        assert_eq!(
            allocation,
            Some(SlotAllocation {
                slot_index: 0,
                newly_assigned: true,
                reassigned_from_other_group: false,
            })
        );

        let row = sqlx::query(
            "SELECT assigned_run_id, assigned_thread_group, thread_id_map::TEXT AS thread_id_map
             FROM auto_queue_slots
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .fetch_one(&pool)
        .await
        .expect("slot row");
        let assigned_run_id: Option<String> =
            row.try_get("assigned_run_id").expect("assigned_run_id");
        let assigned_thread_group: Option<i64> = row
            .try_get("assigned_thread_group")
            .expect("assigned_thread_group");
        let thread_id_map: Option<String> = row.try_get("thread_id_map").expect("thread_id_map");
        assert_eq!(assigned_run_id.as_deref(), Some("run-2"));
        assert_eq!(assigned_thread_group, Some(0));
        let parsed: serde_json::Value =
            serde_json::from_str(thread_id_map.as_deref().unwrap_or("{}"))
                .expect("thread_id_map json");
        assert_eq!(parsed["123"], "thread-slot-0");

        pool.close().await;
        pg_db.drop().await;
    }

    /// Force the bounded-retry exit path by attaching a PG trigger that
    /// silently rejects any UPDATE that would CAS the slot from `NULL ->
    /// run-1`. Mirrors the SQLite `RAISE(IGNORE)` trigger from the previous
    /// in-memory test.
    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_fails_after_bounded_cas_retries() {
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
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-cas-retry', 'run-1', NULL, 'agent-1', 'pending', 1)",
        )
        .execute(&pool)
        .await
        .expect("seed retry entry");

        // Suppress the CAS update via a BEFORE-UPDATE trigger that returns
        // NULL when the helper attempts to claim the slot for `run-1` from a
        // currently-unassigned slot. Returning NULL from a BEFORE trigger
        // skips the row update, so `rows_affected` stays 0 and the helper's
        // bounded retry loop eventually exits with an error.
        sqlx::query(
            "CREATE OR REPLACE FUNCTION test_ignore_slot_claim()
             RETURNS TRIGGER AS $$
             BEGIN
                 IF NEW.assigned_run_id = 'run-1' AND OLD.assigned_run_id IS NULL THEN
                     RETURN NULL;
                 END IF;
                 RETURN NEW;
             END;
             $$ LANGUAGE plpgsql",
        )
        .execute(&pool)
        .await
        .expect("create trigger function");
        sqlx::query(
            "CREATE TRIGGER ignore_slot_claim
             BEFORE UPDATE ON auto_queue_slots
             FOR EACH ROW EXECUTE FUNCTION test_ignore_slot_claim()",
        )
        .execute(&pool)
        .await
        .expect("attach trigger");

        let error = allocate_slot_for_group_agent_pg(&pool, "run-1", 1, "agent-1")
            .await
            .expect_err("forced claim race must terminate with bounded retry error");
        assert!(
            error.contains("slot allocation retry limit exceeded"),
            "expected bounded-retry error, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn release_slot_for_group_agent_pg_clears_only_matching_assignment() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;

        let released = release_slot_for_group_agent_pg(&pool, "run-1", 0, "agent-1", 0)
            .await
            .expect("release matching slot");
        assert_eq!(released, 1);

        let slot = slot_assignment(&pool, "agent-1", 0).await;
        assert_eq!(slot, (None, None));

        let released_again = release_slot_for_group_agent_pg(&pool, "run-1", 0, "agent-1", 0)
            .await
            .expect("release already cleared slot");
        assert_eq!(released_again, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    /// In the PG flow, `update_entry_status_on_pg(..., done)` does NOT touch
    /// `auto_queue_slots` — the slot stays assigned until a downstream
    /// policy hook calls `release_slot_for_group_agent_pg`. This test
    /// verifies that invariant directly: the done transition records the
    /// audit row, completes the entry, and leaves the slot assignment
    /// intact, mirroring what the SQLite test asserted via a synthetic
    /// trigger.
    #[tokio::test]
    async fn terminal_transition_done_defers_slot_release_failures_until_policy_hook_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-rollback', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group)
             VALUES ('entry-rollback', 'run-1', NULL, 'agent-1', 'pending', NULL, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        update_entry_status_on_pg(
            &pool,
            "entry-rollback",
            ENTRY_STATUS_DISPATCHED,
            "test_dispatch",
            &EntryStatusUpdateOptions {
                dispatch_id: Some("dispatch-rollback".to_string()),
                slot_index: Some(0),
            },
        )
        .await
        .expect("dispatch transition");

        update_entry_status_on_pg(
            &pool,
            "entry-rollback",
            ENTRY_STATUS_DONE,
            "test_done_rollback",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("done transition should defer slot release until policy hook");

        let (status, dispatch_id, completed_at) =
            entry_row_status_dispatch_completed(&pool, "entry-rollback").await;
        assert_eq!(status, ENTRY_STATUS_DONE);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-rollback"));
        assert!(completed_at.is_some());

        assert_eq!(run_status(&pool, "run-1").await, "active");
        let slot = slot_assignment(&pool, "agent-1", 0).await;
        assert_eq!(slot.0.as_deref(), Some("run-1"));
        assert_eq!(
            count_transitions(&pool, "entry-rollback").await,
            2,
            "done transition audit must still be recorded"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn slot_has_active_dispatch_ignores_sidecar_dispatches_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ($1, $2, 'dispatched', $3)",
        )
        .bind("dispatch-sidecar")
        .bind("agent-1")
        .bind(
            serde_json::json!({
                "slot_index": 0,
                "sidecar_dispatch": true,
                "phase_gate": {
                    "run_id": "run-1",
                }
            })
            .to_string(),
        )
        .execute(&pool)
        .await
        .expect("seed sidecar dispatch");

        assert!(
            !slot_has_active_dispatch_pg(&pool, "agent-1", 0)
                .await
                .expect("query active sidecar dispatch"),
            "sidecar phase-gate dispatches must not keep a slot occupied"
        );

        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ($1, $2, 'dispatched', $3)",
        )
        .bind("dispatch-primary")
        .bind("agent-1")
        .bind(serde_json::json!({"slot_index": 0}).to_string())
        .execute(&pool)
        .await
        .expect("seed primary dispatch");

        assert!(
            slot_has_active_dispatch_pg(&pool, "agent-1", 0)
                .await
                .expect("query active primary dispatch"),
            "primary dispatches must still block slot reuse"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn slot_has_active_dispatch_ignores_orphaned_review_dispatches_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, dispatch_type, status, context)
             VALUES ($1, $2, 'review', 'dispatched', $3)",
        )
        .bind("dispatch-orphan-review")
        .bind("agent-1")
        .bind(serde_json::json!({"slot_index": 0}).to_string())
        .execute(&pool)
        .await
        .expect("seed orphan review dispatch");

        assert!(
            !slot_has_active_dispatch_pg(&pool, "agent-1", 0)
                .await
                .expect("query orphan review dispatch"),
            "review dispatches without an active provider session must not block slot reuse"
        );

        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, dispatch_type, status, context)
             VALUES ($1, $2, 'review', 'dispatched', $3)",
        )
        .bind("dispatch-live-review")
        .bind("agent-1")
        .bind(serde_json::json!({"slot_index": 0}).to_string())
        .execute(&pool)
        .await
        .expect("seed live review dispatch");
        sqlx::query(
            "INSERT INTO sessions (session_key, agent_id, status, active_dispatch_id)
             VALUES ('session-live-review', 'agent-1', 'turn_active', 'dispatch-live-review')",
        )
        .execute(&pool)
        .await
        .expect("seed live review session");

        assert!(
            slot_has_active_dispatch_pg(&pool, "agent-1", 0)
                .await
                .expect("query live review dispatch"),
            "review dispatches with an active provider session must still block slot reuse"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn allocate_slot_for_group_agent_pg_ignores_orphaned_review_slot_blocker() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, context)
             VALUES ('dispatch-orphan-review-slot', NULL, 'agent-1', 'review', 'dispatched', $1)",
        )
        .bind(
            serde_json::json!({
                "slot_index": 0,
                "review_target_reject_reason": "latest_work_target_issue_mismatch"
            })
            .to_string(),
        )
        .execute(&pool)
        .await
        .expect("seed orphan review slot dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase)
             VALUES ('entry-next-after-orphan-review', 'run-1', NULL, 'agent-1', 'pending', 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("seed pending entry");

        let allocation = allocate_slot_for_group_agent_pg(&pool, "run-1", 0, "agent-1")
            .await
            .expect("allocate slot past orphan review dispatch")
            .expect("existing slot should be reusable");
        assert_eq!(allocation.slot_index, 0);
        assert!(!allocation.newly_assigned);

        let next_slot: Option<i64> = sqlx::query_scalar(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-next-after-orphan-review'",
        )
        .fetch_one(&pool)
        .await
        .expect("next entry slot");
        assert_eq!(next_slot, Some(0));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn record_consultation_dispatch_preserves_metadata_and_marks_entry_dispatched_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, metadata)
             VALUES ('card-consult', 'Card Consult', 'requested', CAST($1 AS jsonb))",
        )
        .bind(
            serde_json::json!({
                "keep": "yes",
                "preflight_status": "consult_required"
            })
            .to_string(),
        )
        .execute(&pool)
        .await
        .expect("seed kanban card");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-consult', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group)
             VALUES ('entry-consult', 'run-1', 'card-consult', 'agent-1', 'pending', 0)",
        )
        .execute(&pool)
        .await
        .expect("seed entry");

        let result = record_consultation_dispatch_on_pg(
            &pool,
            "entry-consult",
            "card-consult",
            "dispatch-consult",
            "test_consultation_dispatch",
            r#"{"keep":"yes","preflight_status":"consult_required"}"#,
        )
        .await
        .expect("consultation dispatch");
        assert!(result.entry_status_changed);

        let metadata: serde_json::Value =
            sqlx::query_scalar("SELECT metadata::TEXT FROM kanban_cards WHERE id = 'card-consult'")
                .fetch_one(&pool)
                .await
                .ok()
                .and_then(|raw: String| serde_json::from_str(&raw).ok())
                .expect("metadata json");
        assert_eq!(metadata["keep"], "yes");
        assert_eq!(metadata["preflight_status"], "consult_required");
        assert_eq!(metadata["consultation_status"], "pending");
        assert_eq!(metadata["consultation_dispatch_id"], "dispatch-consult");

        let row = sqlx::query(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-consult'",
        )
        .fetch_one(&pool)
        .await
        .expect("entry row");
        let status: String = row.try_get("status").expect("status");
        let dispatch_id: Option<String> = row.try_get("dispatch_id").expect("dispatch_id");
        assert_eq!(status, ENTRY_STATUS_DISPATCHED);
        assert_eq!(dispatch_id.as_deref(), Some("dispatch-consult"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn record_consultation_dispatch_requires_dispatch_id_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        let error = record_consultation_dispatch_on_pg(
            &pool,
            "entry-missing",
            "card-missing",
            "   ",
            "test_consultation_dispatch",
            "{}",
        )
        .await
        .expect_err("missing dispatch id must fail");
        assert!(
            error.contains("consultation dispatch id is required"),
            "expected missing-dispatch-id error, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn save_phase_gate_state_filters_invalid_dispatches_and_removes_stale_rows_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        for dispatch_id in &["dispatch-valid-1", "dispatch-valid-2", "dispatch-stale"] {
            sqlx::query(
                "INSERT INTO task_dispatches (id, to_agent_id, status, context)
                 VALUES ($1, 'agent-1', 'dispatched', '{}')",
            )
            .bind(dispatch_id)
            .execute(&pool)
            .await
            .expect("seed dispatch");
        }
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates
                (run_id, phase, status, dispatch_id, pass_verdict)
             VALUES ('run-1', 2, 'pending', 'dispatch-stale', 'phase_gate_passed')",
        )
        .execute(&pool)
        .await
        .expect("seed stale phase gate row");

        let result = save_phase_gate_state_on_pg(
            &pool,
            "run-1",
            2,
            &PhaseGateStateWrite {
                status: "failed".to_string(),
                verdict: Some("phase_gate_failed".to_string()),
                dispatch_ids: vec![
                    "dispatch-valid-1".to_string(),
                    "dispatch-valid-1".to_string(),
                    "dispatch-missing".to_string(),
                    "dispatch-valid-2".to_string(),
                ],
                pass_verdict: "phase_gate_passed".to_string(),
                next_phase: Some(3),
                final_phase: true,
                anchor_card_id: None,
                failure_reason: Some("phase gate failed".to_string()),
                created_at: Some("2026-04-15 00:00:00+00".to_string()),
            },
        )
        .await
        .expect("save phase gate state");

        assert_eq!(
            result.persisted_dispatch_ids,
            vec![
                "dispatch-valid-1".to_string(),
                "dispatch-valid-2".to_string()
            ]
        );
        assert_eq!(result.removed_stale_rows, 1);

        let rows = sqlx::query(
            "SELECT dispatch_id, status, verdict, next_phase, final_phase, failure_reason
             FROM auto_queue_phase_gates
             WHERE run_id = $1 AND phase = $2
             ORDER BY dispatch_id ASC",
        )
        .bind("run-1")
        .bind(2_i64)
        .fetch_all(&pool)
        .await
        .expect("phase gate rows");
        assert_eq!(rows.len(), 2);
        let dispatch_id_0: Option<String> = rows[0].try_get("dispatch_id").expect("dispatch_id 0");
        let dispatch_id_1: Option<String> = rows[1].try_get("dispatch_id").expect("dispatch_id 1");
        let status_0: String = rows[0].try_get("status").expect("status 0");
        let verdict_0: Option<String> = rows[0].try_get("verdict").expect("verdict 0");
        let next_phase_0: Option<i64> = rows[0].try_get("next_phase").expect("next_phase 0");
        let final_phase_0: bool = rows[0].try_get("final_phase").expect("final_phase 0");
        let failure_reason_0: Option<String> =
            rows[0].try_get("failure_reason").expect("failure_reason 0");
        assert_eq!(dispatch_id_0.as_deref(), Some("dispatch-valid-1"));
        assert_eq!(dispatch_id_1.as_deref(), Some("dispatch-valid-2"));
        assert_eq!(status_0, "failed");
        assert_eq!(verdict_0.as_deref(), Some("phase_gate_failed"));
        assert_eq!(next_phase_0, Some(3));
        assert!(final_phase_0);
        assert_eq!(failure_reason_0.as_deref(), Some("phase gate failed"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn clear_phase_gate_state_removes_phase_rows_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates
                (run_id, phase, status, dispatch_id, pass_verdict)
             VALUES ('run-1', 2, 'pending', NULL, 'phase_gate_passed')",
        )
        .execute(&pool)
        .await
        .expect("seed phase 2");
        sqlx::query(
            "INSERT INTO auto_queue_phase_gates
                (run_id, phase, status, dispatch_id, pass_verdict)
             VALUES ('run-1', 3, 'pending', NULL, 'phase_gate_passed')",
        )
        .execute(&pool)
        .await
        .expect("seed phase 3");

        assert!(
            clear_phase_gate_state_on_pg(&pool, "run-1", 2)
                .await
                .expect("clear phase 2")
        );

        let phase_two_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_phase_gates
             WHERE run_id = $1 AND phase = $2",
        )
        .bind("run-1")
        .bind(2_i64)
        .fetch_one(&pool)
        .await
        .expect("phase 2 count");
        let phase_three_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT
             FROM auto_queue_phase_gates
             WHERE run_id = $1 AND phase = $2",
        )
        .bind("run-1")
        .bind(3_i64)
        .fetch_one(&pool)
        .await
        .expect("phase 3 count");
        assert_eq!(phase_two_count, 0);
        assert_eq!(phase_three_count, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    /// #821 (4): `done` entries must not reactivate without an explicit
    /// operator rerun. The shared `update_entry_status_on_pg` helper gates
    /// `done -> dispatched` behind the `pmd_reopen` / `rereview_dispatch`
    /// trigger sources (see `is_allowed_entry_transition`), and `done ->
    /// pending` is simply not in the allowlist. The only authorized entry
    /// point that can legally flip a `done` row back to `dispatched` is
    /// `reactivate_done_entry_on_pg` itself, invoked from the PMD reopen
    /// route in `src/server/routes/kanban.rs` (the `pmd_reopen` /
    /// `rereview_dispatch` call sites).
    #[tokio::test]
    async fn done_entry_cannot_reactivate_without_explicit_operator_rerun_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-821-rea', 'repo-1', 'agent-1', 'completed')",
        )
        .execute(&pool)
        .await
        .expect("seed completed run");
        sqlx::query(
            "INSERT INTO task_dispatches (id, to_agent_id, status, context)
             VALUES ('dispatch-sneak', 'agent-1', 'dispatched', '{}')",
        )
        .execute(&pool)
        .await
        .expect("seed sneak dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, thread_group, completed_at)
             VALUES ('entry-821-rea', 'run-821-rea', NULL, 'agent-1', 'done', 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed done entry");

        for bogus_source in &[
            "auto_queue_tick",
            "tick",
            "policy_hook",
            "onDispatchCompleted",
            "review_automation",
        ] {
            let err = update_entry_status_on_pg(
                &pool,
                "entry-821-rea",
                ENTRY_STATUS_DISPATCHED,
                bogus_source,
                &EntryStatusUpdateOptions {
                    dispatch_id: Some("dispatch-sneak".to_string()),
                    slot_index: Some(0),
                },
            )
            .await
            .expect_err("non-operator source must not resurrect a done entry");
            assert!(
                err.contains("invalid auto-queue entry transition"),
                "source `{bogus_source}` unexpectedly permitted done -> dispatched (got {err})"
            );
        }

        let err = update_entry_status_on_pg(
            &pool,
            "entry-821-rea",
            ENTRY_STATUS_PENDING,
            "pmd_reopen",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect_err("done -> pending must not be a valid transition at all");
        assert!(
            err.contains("invalid auto-queue entry transition"),
            "expected invalid-transition error, got: {err}"
        );

        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'entry-821-rea'")
                .fetch_one(&pool)
                .await
                .expect("entry row");
        assert_eq!(entry_status, "done");
        assert_eq!(run_status(&pool, "run-821-rea").await, "completed");

        let restored = reactivate_done_entry_on_pg(
            &pool,
            "entry-821-rea",
            "pmd_reopen",
            &EntryStatusUpdateOptions::default(),
        )
        .await
        .expect("operator-authorized reactivate must succeed");
        assert!(restored.changed);
        assert_eq!(restored.from_status, ENTRY_STATUS_DONE);
        assert_eq!(restored.to_status, ENTRY_STATUS_DISPATCHED);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn failed_entry_can_reconcile_done_when_card_done_and_commit_recorded_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO agents (id, name) VALUES ('agent-1', 'Agent 1')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(&pool)
        .await
        .expect("seed agent");
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-1866', 'repo-1', 'agent-1', 'active')",
        )
        .execute(&pool)
        .await
        .expect("seed run");
        sqlx::query(
            "INSERT INTO kanban_cards
                (id, repo_id, title, status, assigned_agent_id, latest_dispatch_id, completed_at)
             VALUES ('card-1866', 'repo-1', 'Issue 1866', 'done', 'agent-1', 'dispatch-1866', NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed done card");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, status, result, completed_at)
             VALUES ('dispatch-1866', 'card-1866', 'agent-1', 'completed', $1, NOW())",
        )
        .bind(r#"{"completed_commit":"abc123"}"#)
        .execute(&pool)
        .await
        .expect("seed completed dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, retry_count, thread_group, completed_at)
             VALUES ('entry-1866', 'run-1866', 'card-1866', 'agent-1', 'failed', 1, 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed failed entry");
        sqlx::query(
            "INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id, trigger_source)
             VALUES ('entry-1866', 'dispatch-1866', 'test')",
        )
        .execute(&pool)
        .await
        .expect("seed dispatch history");

        let result =
            reconcile_failed_entry_done_on_pg(&pool, "entry-1866", "manual_terminal_reconcile")
                .await
                .expect("terminal reconciliation should succeed");
        assert!(result.changed);
        assert_eq!(result.from_status, ENTRY_STATUS_FAILED);
        assert_eq!(result.to_status, ENTRY_STATUS_DONE);

        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'entry-1866'")
                .fetch_one(&pool)
                .await
                .expect("entry status");
        assert_eq!(entry_status, ENTRY_STATUS_DONE);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn failed_entry_done_reconcile_requires_completed_commit_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = setup_pool(&pg_db).await;
        sqlx::query(
            "INSERT INTO agents (id, name) VALUES ('agent-1', 'Agent 1')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(&pool)
        .await
        .expect("seed agent");
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-1866-no-commit', 'repo-1', 'agent-1', 'active')",
        )
        .execute(&pool)
        .await
        .expect("seed run");
        sqlx::query(
            "INSERT INTO kanban_cards
                (id, repo_id, title, status, assigned_agent_id, latest_dispatch_id, completed_at)
             VALUES ('card-1866-no-commit', 'repo-1', 'Issue 1866', 'done', 'agent-1', 'dispatch-1866-no-commit', NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed done card");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, status, result, completed_at)
             VALUES ('dispatch-1866-no-commit', 'card-1866-no-commit', 'agent-1', 'completed', $1, NOW())",
        )
        .bind(r#"{"summary":"done"}"#)
        .execute(&pool)
        .await
        .expect("seed completed dispatch without commit");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, retry_count, thread_group, completed_at)
             VALUES ('entry-1866-no-commit', 'run-1866-no-commit', 'card-1866-no-commit', 'agent-1', 'failed', 1, 0, NOW())",
        )
        .execute(&pool)
        .await
        .expect("seed failed entry");

        let error = reconcile_failed_entry_done_on_pg(
            &pool,
            "entry-1866-no-commit",
            "manual_terminal_reconcile",
        )
        .await
        .expect_err("missing completed_commit must block reconciliation");
        assert!(
            error.contains("completed_commit"),
            "expected completed_commit error, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
