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
        seed_run_holding_slot_on_thread(pool, suffix, SLOT_THREAD_ID).await
    }

    /// Thread-parameterised variant. The recovery-done latch that
    /// `drain_with_health_registry_tears_down_provider_runtime_pg` observes is a
    /// process-global map keyed by channel id, so that test needs a thread id no
    /// other test in this binary touches.
    async fn seed_run_holding_slot_on_thread(
        pool: &PgPool,
        suffix: &str,
        slot_thread_id: &str,
    ) -> (String, String) {
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
        .bind(slot_thread_id)
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
        .bind(slot_thread_id)
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

    /// Make every pending task eligible again. Tests that deliberately fail a
    /// drain need this because the failure arms a real backoff.
    async fn wind_back_next_attempt(pool: &PgPool) {
        sqlx::query("UPDATE auto_queue_run_cleanup_tasks SET next_attempt_at = NOW()")
            .execute(pool)
            .await
            .expect("wind back cleanup backoff"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    }

    async fn task_retry_state(pool: &PgPool, id: i64) -> (i32, Option<String>, bool) {
        sqlx::query_as::<_, (i32, Option<String>, bool)>(
            "SELECT attempts, last_error, dead_lettered_at IS NOT NULL
             FROM auto_queue_run_cleanup_tasks WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("load cleanup retry state") // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
    }

    /// Reject any UPDATE that changes `released_slots`, so the durable half of
    /// the slot release fails while the `auto_queue_slots` UPDATE beside it
    /// succeeds. This is the injection that opens the #5142 D-1 crash window.
    async fn arm_released_slots_persist_failure(pool: &PgPool) {
        sqlx::query(
            "CREATE OR REPLACE FUNCTION reject_released_slots_persist()
             RETURNS trigger AS $$
             BEGIN
                 IF NEW.released_slots IS DISTINCT FROM OLD.released_slots THEN
                     RAISE EXCEPTION 'injected released_slots persist failure';
                 END IF;
                 RETURN NEW;
             END;
             $$ LANGUAGE plpgsql",
        )
        .execute(pool)
        .await
        .expect("define released_slots persist trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "CREATE TRIGGER reject_released_slots_persist_trigger
             BEFORE UPDATE ON auto_queue_run_cleanup_tasks
             FOR EACH ROW EXECUTE FUNCTION reject_released_slots_persist()",
        )
        .execute(pool)
        .await
        .expect("arm released_slots persist trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    }

    async fn disarm_released_slots_persist_failure(pool: &PgPool) {
        sqlx::query(
            "DROP TRIGGER reject_released_slots_persist_trigger
             ON auto_queue_run_cleanup_tasks",
        )
        .execute(pool)
        .await
        .expect("disarm released_slots persist trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
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

        // Step 1 ran before the failure, and its idempotency key must be durable.
        // `emit()` has no dedup key of its own, so if this flag were not
        // committed the retry below would fire the same observability rows a
        // second time.
        let emitted = sqlx::query_scalar::<_, bool>(
            "SELECT emitted FROM auto_queue_run_cleanup_tasks WHERE id = $1",
        )
        .bind(cancelled.cleanup_task_id)
        .fetch_one(&pool)
        .await
        .expect("load emitted flag"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
        assert!(
            emitted,
            "the emit must be durably marked so the retry cannot repeat it"
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

        // #5142 D-2: the failure armed an exponential backoff, so the very next
        // sweep must decline to pick the row up. Without this the queue would
        // spin on a failing row at full tick rate and keep its head-of-line
        // position against everything queued behind it.
        let backed_off = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay while backing off"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(
            backed_off,
            RunCleanupReplayStats::default(),
            "a task inside its backoff window must not be drained"
        );
        assert_eq!(
            slot_assignment(&pool).await,
            Some(run_id.clone()),
            "the backed-off task must not have run any step"
        );

        wind_back_next_attempt(&pool).await;
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

    // ---------------------------------------------------------------------
    // #5142 round 2 — discriminating tests for the claims this PR rests on.
    // ---------------------------------------------------------------------

    /// **The P0 claim under test.** `enqueue_run_cleanup_task_on_tx` is only a
    /// fix because it runs inside the transaction that commits the cancel. Move
    /// that INSERT into a transaction of its own after the commit and the defect
    /// comes straight back: the cancel is durable while the record that cleanup
    /// is owed is not, so a crash in between loses the cleanup with no trace.
    ///
    /// The window is opened by failing the INSERT (the cleanup table is renamed
    /// out from under it) and then asking the only question that separates the
    /// two shapes: **did the state change roll back with it?**
    ///
    /// - same transaction  → INSERT aborts the transaction → dispatch stays
    ///   `dispatched`, slot stays held, no cleanup row.
    /// - separate transaction after the commit → the cancel is already committed
    ///   → dispatch is `cancelled` with no cleanup row, i.e. exactly the
    ///   unrecoverable state this PR claims to have eliminated.
    #[tokio::test]
    async fn enqueue_is_atomic_with_the_state_change_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, dispatch_id) = seed_run_holding_slot(&pool, "atomic").await;

        sqlx::query(
            "ALTER TABLE auto_queue_run_cleanup_tasks
             RENAME TO auto_queue_run_cleanup_tasks_hidden",
        )
        .execute(&pool)
        .await
        .expect("hide cleanup task table"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        let failed =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel").await;
        assert!(
            failed.is_err(),
            "a cancel that cannot record its cleanup debt must fail loudly, got {failed:?}"
        );

        // The discriminator: the state change must have died with the INSERT.
        assert_eq!(
            dispatch_status(&pool, &dispatch_id).await,
            "dispatched",
            "the dispatch cancel must roll back with the cleanup record — if it \
             committed, the cleanup row is being written outside the state-change \
             transaction and a crash in that window loses the cleanup forever"
        );
        assert_eq!(
            slot_assignment(&pool).await,
            Some(run_id.clone()),
            "the slot must still be held by the run whose cancel rolled back"
        );

        sqlx::query(
            "ALTER TABLE auto_queue_run_cleanup_tasks_hidden
             RENAME TO auto_queue_run_cleanup_tasks",
        )
        .execute(&pool)
        .await
        .expect("restore cleanup task table"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        // Nothing was left owed, and nothing was left stranded: a retry now
        // behaves exactly like a first attempt.
        assert_eq!(
            pending_run_cleanup_task_count_pg(&pool)
                .await
                .expect("count cleanup tasks"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
            0,
            "a rolled-back cancel must not leave a cleanup row behind"
        );
        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel after the cleanup table came back"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(cancelled.dispatch_ids, vec![dispatch_id.clone()]);
        assert_eq!(dispatch_status(&pool, &dispatch_id).await, "cancelled");
        assert!(
            drain_run_cleanup_task_by_id_pg(None, &pool, cancelled.cleanup_task_id)
                .await
                .completed
        );
        assert_eq!(slot_assignment(&pool).await, None);
        assert_eq!(provider_session_ids(&pool).await, vec![None]);
    }

    /// **#5142 D-1 regression.** The slot release and the durable record of
    /// which slots were released must commit together.
    ///
    /// Injecting a failure into the `released_slots` write reproduces the crash
    /// window that the reviewer's probe hit. Two independent assertions separate
    /// the atomic shape from the two-commit shape:
    ///
    /// 1. Immediately after the failed drain the slot must still be **held**.
    ///    Under two commits the `auto_queue_slots` UPDATE has already committed
    ///    on its own, so the slot reads `NULL`.
    /// 2. After the injection is removed, the replay must fully converge. Under
    ///    two commits the replay finds the slots already released, merges that
    ///    into an empty persisted set, iterates nothing in step 4, and deletes
    ///    the row while reporting `completed` — leaving the residual provider
    ///    session id behind and destroying the retry evidence.
    #[tokio::test]
    async fn slot_release_and_its_durable_record_commit_together_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, _dispatch_id) = seed_run_holding_slot(&pool, "atomicslots").await;

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        arm_released_slots_persist_failure(&pool).await;
        let outcome = drain_run_cleanup_task_by_id_pg(None, &pool, cancelled.cleanup_task_id).await;
        assert!(
            !outcome.completed,
            "a drain whose slot bookkeeping failed must not report success: {outcome:?}"
        );

        // Discriminator 1 — the release must have rolled back with its record.
        assert_eq!(
            slot_assignment(&pool).await,
            Some(run_id.clone()),
            "the slot release must roll back together with the released_slots \
             write — a released slot here means the two UPDATEs committed \
             separately and a crash between them is possible"
        );
        assert_eq!(
            pending_run_cleanup_task_count_pg(&pool)
                .await
                .expect("count cleanup tasks"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
            1,
            "the failed task must stay on disk so it can be retried"
        );
        assert_eq!(
            provider_session_ids(&pool).await,
            vec![Some("claude-session-atomicslots".to_string())],
            "nothing downstream may have run"
        );

        disarm_released_slots_persist_failure(&pool).await;
        wind_back_next_attempt(&pool).await;

        // Discriminator 2 — the retry converges completely, and `completed`
        // means what it says.
        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay pending cleanup tasks"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(stats.drained, 1);
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.dead_lettered, 0);
        assert_eq!(
            slot_assignment(&pool).await,
            None,
            "the retry must release the slot"
        );
        assert_eq!(
            provider_session_ids(&pool).await,
            vec![None],
            "a task reported as completed must actually have cleared the \
             residual provider session id — reporting completed while the \
             slot-thread clear was skipped is exactly the #5142 defect"
        );
        assert_eq!(
            pending_run_cleanup_task_count_pg(&pool)
                .await
                .expect("count cleanup tasks"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
            0
        );
    }

    /// **#5142 D-3.** Step 2 of the drain is a structural no-op, and this pins
    /// that so it is never mistaken for the step that clears
    /// `claude_session_id`.
    ///
    /// The cancel transaction already runs `UPDATE sessions SET
    /// active_dispatch_id = NULL WHERE active_dispatch_id = $2`, so the
    /// post-commit `clear_sessions_for_dispatches_pg` — whose predicate is that
    /// same `active_dispatch_id` — can never match a row. The provider session
    /// id is actually cleared by step 4, through the slot's thread bindings.
    #[tokio::test]
    async fn session_clear_is_a_structural_no_op_after_the_cancel_commit_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, dispatch_id) = seed_run_holding_slot(&pool, "noop").await;

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(cancelled.dispatch_ids, vec![dispatch_id.clone()]);

        // The committed cancel already unbound the session from the dispatch...
        let still_bound = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sessions WHERE active_dispatch_id = $1",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("count sessions still bound to the dispatch"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
        assert_eq!(
            still_bound, 0,
            "the cancel transaction already cleared active_dispatch_id"
        );

        // ...so step 2 has nothing left to match, on either production path.
        let cleared = clear_sessions_for_dispatches_pg(&pool, &cancelled.dispatch_ids)
            .await
            .expect("run the post-commit session clear"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(
            cleared, 0,
            "step 2 is structurally a no-op after the cancel commit; it is kept \
             as a retry gate, not as the step that clears the provider session"
        );
        assert_eq!(
            provider_session_ids(&pool).await,
            vec![Some("claude-session-noop".to_string())],
            "and it does not clear claude_session_id — step 4 does"
        );

        // Prove the attribution: only the full drain (step 4 included) clears it.
        assert!(
            drain_run_cleanup_task_by_id_pg(None, &pool, cancelled.cleanup_task_id)
                .await
                .completed
        );
        assert_eq!(provider_session_ids(&pool).await, vec![None]);
    }

    /// **#5142 D-4.** The health registry is not decoration: the slot-thread
    /// clear owes a runtime-side teardown that only exists when a registry is
    /// present. Passing `None` skips `clear_provider_channel_runtime` entirely.
    ///
    /// The teardown's observable trace is the per-channel recovery-done latch
    /// that `mailbox_clear_channel` marks. With a registered provider runtime it
    /// appears; with `None` it is never created at all.
    #[tokio::test]
    async fn drain_with_health_registry_tears_down_provider_runtime_pg() {
        use crate::services::turn_orchestrator::ChannelMailboxRegistry;

        // A thread id no other test in this binary uses: the latch map is
        // process-global and keyed by channel id.
        const RUNTIME_THREAD_ID: &str = "5142000000001";
        let channel_id = poise::serenity_prelude::ChannelId::new(
            RUNTIME_THREAD_ID
                .parse::<u64>()
                .expect("runtime thread id is numeric"), // agentdesk-audit: allow-unwrap — test-only constant
        );
        assert!(
            ChannelMailboxRegistry::global_recovery_done(channel_id).is_none(),
            "precondition: nothing has touched this channel's runtime yet"
        );

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, _dispatch_id) =
            seed_run_holding_slot_on_thread(&pool, "runtime", RUNTIME_THREAD_ID).await;

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        let registry = std::sync::Arc::new(crate::services::discord::health::HealthRegistry::new());
        registry
            .register(
                "claude".to_string(),
                crate::services::discord::make_shared_data_for_tests(),
            )
            .await;

        let outcome =
            drain_run_cleanup_task_by_id_pg(Some(registry), &pool, cancelled.cleanup_task_id).await;
        assert!(outcome.completed, "drain must finish: {outcome:?}");
        assert_eq!(outcome.slot_cleanup.released_slots, 1);

        // The teardown is spawned, so poll for its trace rather than assuming
        // it already ran.
        let mut observed = None;
        for _ in 0..100 {
            if let Some(signal) = ChannelMailboxRegistry::global_recovery_done(channel_id) {
                observed = Some(signal);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            observed.is_some(),
            "the registered provider runtime must have been torn down for the \
             cleared slot thread — passing None here silently drops the runtime \
             half of the cleanup"
        );
    }

    /// **#5142 D-2.** A task that can never succeed must stop occupying the head
    /// of the drain order, and a task that cannot even be decoded must be
    /// reported rather than silently skipped.
    #[tokio::test]
    async fn poison_and_exhausted_tasks_dead_letter_instead_of_blocking_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        // An older, undecodable row sits ahead of everything else.
        let poison_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO auto_queue_run_cleanup_tasks
                (run_ids, dispatch_ids, released_slots, pending_emits, created_at)
             VALUES ('{}', '{}', '[]'::jsonb, '\"not-an-array\"'::jsonb,
                     NOW() - INTERVAL '1 hour')
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("seed undecodable cleanup task"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        let (run_id, _dispatch_id) = seed_run_holding_slot(&pool, "poison").await;
        cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
            .await
            .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay pending cleanup tasks"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(
            stats.dead_lettered, 1,
            "the undecodable row must be reported, not silently skipped"
        );
        assert_eq!(
            stats.completed, 1,
            "the healthy task queued behind it must still drain"
        );
        assert_eq!(slot_assignment(&pool).await, None);
        assert_eq!(provider_session_ids(&pool).await, vec![None]);
        assert!(
            task_retry_state(&pool, poison_id).await.2,
            "the poison row must be parked, not deleted — the evidence stays"
        );

        // And once parked it is out of the way for good.
        let after = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay after dead-lettering"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(after, RunCleanupReplayStats::default());
    }

    /// **#5142 D-2.** A task that keeps failing must be dead-lettered once it
    /// burns through the attempt cap, instead of retrying forever.
    #[tokio::test]
    async fn repeatedly_failing_task_dead_letters_at_the_attempt_cap_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, _dispatch_id) = seed_run_holding_slot(&pool, "cap").await;

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        // Fast-forward to the last attempt this task is entitled to.
        sqlx::query("UPDATE auto_queue_run_cleanup_tasks SET attempts = $1 WHERE id = $2")
            .bind(MAX_CLEANUP_ATTEMPTS - 1)
            .bind(cancelled.cleanup_task_id)
            .execute(&pool)
            .await
            .expect("fast-forward attempts"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        sqlx::query("ALTER TABLE sessions RENAME TO sessions_hidden")
            .execute(&pool)
            .await
            .expect("hide sessions table"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        let outcome = drain_run_cleanup_task_by_id_pg(None, &pool, cancelled.cleanup_task_id).await;
        assert!(!outcome.completed);
        sqlx::query("ALTER TABLE sessions_hidden RENAME TO sessions")
            .execute(&pool)
            .await
            .expect("restore sessions table"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        let (attempts, last_error, dead_lettered) =
            task_retry_state(&pool, cancelled.cleanup_task_id).await;
        assert_eq!(attempts, MAX_CLEANUP_ATTEMPTS);
        assert!(last_error.is_some_and(|error| error.contains("clear postgres sessions")));
        assert!(
            dead_lettered,
            "a task past the attempt cap must be dead-lettered"
        );

        // Dead-lettered rows are invisible to both drain paths, even with the
        // backoff wound back — otherwise they would block the queue forever.
        wind_back_next_attempt(&pool).await;
        assert_eq!(
            replay_pending_run_cleanup_tasks_pg(None, &pool)
                .await
                .expect("replay after dead-lettering"), // agentdesk-audit: allow-unwrap — production entrypoint assertion
            RunCleanupReplayStats::default()
        );
        assert!(
            !drain_run_cleanup_task_by_id_pg(None, &pool, cancelled.cleanup_task_id)
                .await
                .completed,
            "a dead-lettered task must never be reported as completed"
        );
        assert_eq!(
            pending_run_cleanup_task_count_pg(&pool)
                .await
                .expect("count cleanup tasks"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
            1,
            "the dead-lettered row is retained for the operator"
        );
    }

    /// **#5142 D-6.** The inline post-commit drain and the tick replay sweep both
    /// target live rows. The row claim is what stops them from running the same
    /// task twice and firing its observability emits twice.
    #[tokio::test]
    async fn a_claimed_task_is_not_drained_twice_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, _dispatch_id) = seed_run_holding_slot(&pool, "claim").await;

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        // Model the competing drainer: it holds a fresh claim on the row.
        sqlx::query(
            "UPDATE auto_queue_run_cleanup_tasks
             SET claim_owner = 'other-drainer', claimed_at = NOW()
             WHERE id = $1",
        )
        .bind(cancelled.cleanup_task_id)
        .execute(&pool)
        .await
        .expect("simulate a competing claim"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay while the row is claimed"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(
            stats,
            RunCleanupReplayStats::default(),
            "the sweep must not touch a row another drainer owns"
        );

        let outcome = drain_run_cleanup_task_by_id_pg(None, &pool, cancelled.cleanup_task_id).await;
        assert!(
            !outcome.completed,
            "a row we could not claim is still owed, so it must not be reported \
             as completed"
        );
        assert_eq!(
            slot_assignment(&pool).await,
            Some(run_id.clone()),
            "no step may run without the claim"
        );

        // Once the lease expires the row becomes drainable again, so a drainer
        // that died holding a claim cannot strand it.
        sqlx::query(
            "UPDATE auto_queue_run_cleanup_tasks
             SET claimed_at = NOW() - ($1::BIGINT * INTERVAL '1 second')
             WHERE id = $2",
        )
        .bind(CLAIM_LEASE_SECONDS + 60)
        .bind(cancelled.cleanup_task_id)
        .execute(&pool)
        .await
        .expect("expire the competing claim"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        let recovered = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay after the lease expired"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(recovered.completed, 1);
        assert_eq!(slot_assignment(&pool).await, None);
        assert_eq!(provider_session_ids(&pool).await, vec![None]);
    }
}
