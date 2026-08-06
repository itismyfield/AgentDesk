// #5142: every test here needs a live PostgreSQL server, so the module name
// carries the `pg_` marker the PG test lane selects on.
#[cfg(test)]
mod pg_tests {
    use super::super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::auto_queue::cancel_run::cancel_live_dispatches_for_runs_pg;
    use sqlx::PgPool;

    const SLOT_THREAD_ID: &str = "7142";

    /// Seed one active run holding slot 0, one dispatched entry, one live
    /// dispatch, and a provider session bound to both the dispatch and the
    /// slot's thread.
    async fn seed_run_holding_slot(pool: &PgPool, suffix: &str) -> (String, String) {
        let run_id = format!("run-cleanup-{suffix}");
        let dispatch_id = format!("dispatch-cleanup-{suffix}");
        let card_id = format!("card-cleanup-{suffix}");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-cleanup', 'Cleanup Agent', 'claude', '123')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(pool)
        .await
        .expect("seed cleanup agent"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ($1, 'Cleanup Card', 'in_progress', 'agent-cleanup')",
        )
        .bind(&card_id)
        .execute(pool)
        .await
        .expect("seed cleanup card"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, agent_id, status)
             VALUES ($1, 'agent-cleanup', 'active')",
        )
        .bind(&run_id)
        .execute(pool)
        .await
        .expect("seed cleanup run"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
             VALUES ($1, $2, 'agent-cleanup', 'implementation', 'dispatched', 'Cleanup Dispatch')",
        )
        .bind(&dispatch_id)
        .bind(&card_id)
        .execute(pool)
        .await
        .expect("seed cleanup dispatch"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index)
             VALUES ($1, $2, $3, 'agent-cleanup', 'dispatched', $4, 0)",
        )
        .bind(format!("entry-cleanup-{suffix}"))
        .bind(&run_id)
        .bind(&card_id)
        .bind(&dispatch_id)
        .execute(pool)
        .await
        .expect("seed cleanup entry"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
             VALUES ('agent-cleanup', 0, $1, 0, jsonb_build_object('0', $2::text))",
        )
        .bind(&run_id)
        .bind(SLOT_THREAD_ID)
        .execute(pool)
        .await
        .expect("seed cleanup slot"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "INSERT INTO sessions (
                session_key, provider, status, active_dispatch_id, session_info,
                tokens, thread_channel_id, claude_session_id
             )
             VALUES ($1, 'claude', 'idle', $2, 'before cleanup', 17, $3, $4)",
        )
        .bind(format!("session-cleanup-{suffix}"))
        .bind(&dispatch_id)
        .bind(SLOT_THREAD_ID)
        .bind(format!("claude-session-{suffix}"))
        .execute(pool)
        .await
        .expect("seed cleanup session"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        (run_id, dispatch_id)
    }

    async fn slot_assignment(pool: &PgPool) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT assigned_run_id FROM auto_queue_slots
             WHERE agent_id = 'agent-cleanup' AND slot_index = 0",
        )
        .fetch_one(pool)
        .await
        .expect("load slot assignment") // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
    }

    async fn provider_session_ids(pool: &PgPool) -> Vec<Option<String>> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT claude_session_id FROM sessions
             WHERE session_key LIKE 'session-cleanup-%' ORDER BY session_key",
        )
        .fetch_all(pool)
        .await
        .expect("load provider session ids") // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
    }

    async fn dispatch_status(pool: &PgPool, dispatch_id: &str) -> String {
        sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_one(pool)
            .await
            .expect("load dispatch status") // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
    }

    /// Acceptance criterion: crash injected in the window between the cancel
    /// commit and the post-commit cleanup must converge after restart.
    ///
    /// The crash is injected by calling `cancel_live_dispatches_for_runs_pg`
    /// (which commits and returns) and then never draining — exactly the state a
    /// process that died on the next instruction leaves behind. The restart is
    /// modelled by `replay_pending_run_cleanup_tasks_pg`, the same function the
    /// policy tick calls.
    #[tokio::test]
    async fn crash_after_cancel_commit_converges_on_replay_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, dispatch_id) = seed_run_holding_slot(&pool, "crash").await;

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(cancelled.dispatch_ids, vec![dispatch_id.clone()]);

        // --- the crash window itself ---------------------------------------
        // The dispatch cancel is durable, but nothing after the commit ran.
        assert_eq!(dispatch_status(&pool, &dispatch_id).await, "cancelled");
        assert_eq!(
            slot_assignment(&pool).await,
            Some(run_id.clone()),
            "slot token must still be held before the replay runs"
        );
        assert_eq!(
            provider_session_ids(&pool).await,
            vec![Some("claude-session-crash".to_string())],
            "provider session id must still be residual before the replay runs"
        );
        assert_eq!(
            pending_run_cleanup_task_count_pg(&pool)
                .await
                .expect("count cleanup tasks"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
            1,
            "the committed transaction must have left a durable cleanup record"
        );

        // --- restart ---------------------------------------------------------
        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay pending cleanup tasks"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(stats.drained, 1);
        assert_eq!(stats.completed, 1);

        assert_eq!(
            slot_assignment(&pool).await,
            None,
            "restart must release the slot token"
        );
        assert_eq!(
            provider_session_ids(&pool).await,
            vec![None],
            "restart must clear the residual provider session id"
        );
        assert_eq!(
            pending_run_cleanup_task_count_pg(&pool)
                .await
                .expect("count cleanup tasks"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
            0,
            "a converged cleanup task must not be replayed again"
        );
    }

    /// Acceptance criterion: a failing `clear_sessions_for_dispatches_pg` must
    /// stay retry-eligible instead of ending as a warning string.
    ///
    /// The failure is injected by renaming `sessions` out from under the UPDATE
    /// inside this test's own database.
    #[tokio::test]
    async fn session_clear_failure_stays_retry_eligible_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, _dispatch_id) = seed_run_holding_slot(&pool, "retry").await;

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        sqlx::query("ALTER TABLE sessions RENAME TO sessions_hidden")
            .execute(&pool)
            .await
            .expect("hide sessions table"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        let outcome = drain_run_cleanup_task_by_id_pg(None, &pool, cancelled.cleanup_task_id).await;
        assert!(
            !outcome.completed,
            "a failed session clear must not report the task as finished"
        );
        assert!(
            outcome
                .slot_cleanup
                .warnings
                .iter()
                .any(|warning| warning.contains("clear postgres sessions")),
            "the warning is still surfaced: {:?}",
            outcome.slot_cleanup.warnings
        );

        let (attempts, last_error) = sqlx::query_as::<_, (i32, Option<String>)>(
            "SELECT attempts, last_error FROM auto_queue_run_cleanup_tasks WHERE id = $1",
        )
        .bind(cancelled.cleanup_task_id)
        .fetch_one(&pool)
        .await
        .expect("load retry state"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
        assert_eq!(attempts, 1, "the failure must be recorded as an attempt");
        assert!(
            last_error.is_some_and(|error| error.contains("clear postgres sessions")),
            "the failure cause must be retained on the retry record"
        );
        assert_eq!(
            slot_assignment(&pool).await,
            Some(run_id.clone()),
            "a failed session clear must not let the task proceed to slot release"
        );

        sqlx::query("ALTER TABLE sessions_hidden RENAME TO sessions")
            .execute(&pool)
            .await
            .expect("restore sessions table"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay pending cleanup tasks"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(
            stats.completed, 1,
            "the retry must converge once PG recovers"
        );
        assert_eq!(provider_session_ids(&pool).await, vec![None]);
        assert_eq!(slot_assignment(&pool).await, None);
        assert_eq!(
            pending_run_cleanup_task_count_pg(&pool)
                .await
                .expect("count cleanup tasks"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
            0
        );
    }

    /// P2 fixture gap: run the slot-thread clearing branch with a health
    /// registry actually present. The two pre-existing PG tests both passed
    /// `None`, so the `Some(..)` arm of `clear_slot_threads_for_slot_pg` was
    /// never executed by any test.
    #[tokio::test]
    async fn drain_with_health_registry_clears_slot_thread_sessions_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, _dispatch_id) = seed_run_holding_slot(&pool, "registry").await;

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        let registry = std::sync::Arc::new(crate::services::discord::health::HealthRegistry::new());
        let outcome =
            drain_run_cleanup_task_by_id_pg(Some(registry), &pool, cancelled.cleanup_task_id).await;

        assert!(outcome.completed, "drain must finish: {outcome:?}");
        assert_eq!(
            outcome.slot_cleanup.released_slots, 1,
            "the run's slot must be released by the drain"
        );
        assert!(
            outcome.slot_cleanup.cleared_slot_sessions >= 1,
            "the slot-thread clearing branch must have run: {outcome:?}"
        );
        assert_eq!(slot_assignment(&pool).await, None);
        assert_eq!(provider_session_ids(&pool).await, vec![None]);
    }

    /// A replay that arrives after the slot was handed to a different run must
    /// not clear the new owner's slot threads.
    #[tokio::test]
    async fn replay_skips_slot_threads_after_slot_reassignment_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, _dispatch_id) = seed_run_holding_slot(&pool, "aba").await;

        cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
            .await
            .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        // Another run takes the slot while the cleanup is still owed, and brings
        // its own live session on the same thread.
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, agent_id, status)
             VALUES ('run-cleanup-successor', 'agent-cleanup', 'active')",
        )
        .execute(&pool)
        .await
        .expect("seed successor run"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "UPDATE auto_queue_slots
             SET assigned_run_id = 'run-cleanup-successor'
             WHERE agent_id = 'agent-cleanup' AND slot_index = 0",
        )
        .execute(&pool)
        .await
        .expect("reassign slot to successor run"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "INSERT INTO sessions (
                session_key, provider, status, active_dispatch_id, session_info,
                tokens, thread_channel_id, claude_session_id
             )
             VALUES ('session-successor', 'claude', 'idle', 'dispatch-successor',
                     'successor session', 5, $1, 'claude-session-successor')",
        )
        .bind(SLOT_THREAD_ID)
        .execute(&pool)
        .await
        .expect("seed successor session"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        // Model the crash that lands *after* the slot release was persisted:
        // without this the drain's release CAS alone would already keep the
        // replay away from the slot, and the ownership guard would never be
        // reached.
        sqlx::query(
            "UPDATE auto_queue_run_cleanup_tasks
             SET released_slots = jsonb_build_array(
                 jsonb_build_object('agent_id', 'agent-cleanup', 'slot_index', 0)
             )",
        )
        .execute(&pool)
        .await
        .expect("persist released slot on the pending task"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay pending cleanup tasks"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(stats.completed, 1);

        assert_eq!(
            slot_assignment(&pool).await,
            Some("run-cleanup-successor".to_string()),
            "the replay must not steal the slot back from the successor run"
        );
        let successor_session = sqlx::query_scalar::<_, Option<String>>(
            "SELECT claude_session_id FROM sessions WHERE session_key = 'session-successor'",
        )
        .fetch_one(&pool)
        .await
        .expect("load successor session"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
        assert_eq!(
            successor_session,
            Some("claude-session-successor".to_string()),
            "the replay must not clear the successor run's slot-thread session"
        );
    }
}
