// #5142: every test here needs a live PostgreSQL server, so the module name
// carries the `pg_` marker the PG test lane selects on.
#[cfg(test)]
mod pg_tests {
    use super::super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::auto_queue::cancel_run::{
        cancel_live_dispatches_for_runs_pg, cancel_selected_runs_with_pg, end_run_with_pg,
    };
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

    /// Reject exactly one statement: an UPDATE that re-marks an already-emitted
    /// row as emitted while touching nothing else.
    ///
    /// The drain's emit step is `UPDATE .. SET emitted = TRUE, updated_at =
    /// NOW()` and nothing more, so the extra predicates below let every other
    /// writer through — the claim (changes `claim_owner`), a recorded failure
    /// (changes `attempts`) and the slot-release bookkeeping (changes
    /// `released_slots`) are all untouched. What is left is a faithful trap for
    /// "this drain is about to fire the emits again", because the mark now
    /// precedes `emit()`.
    ///
    /// **The claim exemption is conditional, not universal.** It relies on
    /// `claim_owner` actually changing. A *re*-claim by the same process after a
    /// lease expiry writes `pid:N` over `pid:N`, which is `NOT DISTINCT`, and if
    /// that row is already `emitted` with unchanged `attempts` and
    /// `released_slots` the claim UPDATE trips this trap too. No test here
    /// reaches that state — every armed test claims from a fresh row or from a
    /// row another owner tag holds — but a future test that re-claims in-process
    /// would get a false positive, so arm this trap only around a single claim.
    async fn arm_emit_remark_trap(pool: &PgPool) {
        sqlx::query(
            "CREATE OR REPLACE FUNCTION reject_emit_remark()
             RETURNS trigger AS $$
             BEGIN
                 IF OLD.emitted AND NEW.emitted
                    AND NEW.attempts = OLD.attempts
                    AND NEW.claim_owner IS NOT DISTINCT FROM OLD.claim_owner
                    AND NEW.released_slots IS NOT DISTINCT FROM OLD.released_slots THEN
                     RAISE EXCEPTION 'injected re-mark of an already-emitted cleanup task';
                 END IF;
                 RETURN NEW;
             END;
             $$ LANGUAGE plpgsql",
        )
        .execute(pool)
        .await
        .expect("define emit re-mark trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "CREATE TRIGGER reject_emit_remark_trigger
             BEFORE UPDATE ON auto_queue_run_cleanup_tasks
             FOR EACH ROW EXECUTE FUNCTION reject_emit_remark()",
        )
        .execute(pool)
        .await
        .expect("arm emit re-mark trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    }

    async fn disarm_emit_remark_trap(pool: &PgPool) {
        sqlx::query(
            "DROP TRIGGER IF EXISTS reject_emit_remark_trigger ON auto_queue_run_cleanup_tasks",
        )
        .execute(pool)
        .await
        .expect("disarm emit re-mark trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
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

    /// Reject the `DELETE` that retires a finished task, leaving every drain step
    /// successful and only the retirement write broken.
    async fn arm_cleanup_task_delete_failure(pool: &PgPool) {
        sqlx::query(
            "CREATE OR REPLACE FUNCTION reject_cleanup_task_delete()
             RETURNS trigger AS $$
             BEGIN
                 RAISE EXCEPTION 'injected cleanup task delete failure';
             END;
             $$ LANGUAGE plpgsql",
        )
        .execute(pool)
        .await
        .expect("define cleanup delete trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "CREATE TRIGGER reject_cleanup_task_delete_trigger
             BEFORE DELETE ON auto_queue_run_cleanup_tasks
             FOR EACH ROW EXECUTE FUNCTION reject_cleanup_task_delete()",
        )
        .execute(pool)
        .await
        .expect("arm cleanup delete trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    }

    async fn disarm_cleanup_task_delete_failure(pool: &PgPool) {
        sqlx::query(
            "DROP TRIGGER reject_cleanup_task_delete_trigger
             ON auto_queue_run_cleanup_tasks",
        )
        .execute(pool)
        .await
        .expect("disarm cleanup delete trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    }

    /// Reject only the write that parks a row, so `dead_letter_task_pg` fails
    /// while the ordinary attempt bookkeeping beside it still lands.
    async fn arm_dead_letter_write_failure(pool: &PgPool) {
        sqlx::query(
            "CREATE OR REPLACE FUNCTION reject_dead_letter_write()
             RETURNS trigger AS $$
             BEGIN
                 IF NEW.dead_lettered_at IS NOT NULL
                    AND OLD.dead_lettered_at IS NULL THEN
                     RAISE EXCEPTION 'injected dead-letter write failure';
                 END IF;
                 RETURN NEW;
             END;
             $$ LANGUAGE plpgsql",
        )
        .execute(pool)
        .await
        .expect("define dead-letter write trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "CREATE TRIGGER reject_dead_letter_write_trigger
             BEFORE UPDATE ON auto_queue_run_cleanup_tasks
             FOR EACH ROW EXECUTE FUNCTION reject_dead_letter_write()",
        )
        .execute(pool)
        .await
        .expect("arm dead-letter write trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    }

    async fn disarm_dead_letter_write_failure(pool: &PgPool) {
        sqlx::query(
            "DROP TRIGGER reject_dead_letter_write_trigger
             ON auto_queue_run_cleanup_tasks",
        )
        .execute(pool)
        .await
        .expect("disarm dead-letter write trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    }

    /// Reject the attempt bookkeeping itself, i.e. any UPDATE that moves
    /// `attempts`. The claim and the emit mark leave it alone and still pass.
    async fn arm_attempt_record_failure(pool: &PgPool) {
        sqlx::query(
            "CREATE OR REPLACE FUNCTION reject_attempt_record()
             RETURNS trigger AS $$
             BEGIN
                 IF NEW.attempts IS DISTINCT FROM OLD.attempts THEN
                     RAISE EXCEPTION 'injected attempt bookkeeping failure';
                 END IF;
                 RETURN NEW;
             END;
             $$ LANGUAGE plpgsql",
        )
        .execute(pool)
        .await
        .expect("define attempt record trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "CREATE TRIGGER reject_attempt_record_trigger
             BEFORE UPDATE ON auto_queue_run_cleanup_tasks
             FOR EACH ROW EXECUTE FUNCTION reject_attempt_record()",
        )
        .execute(pool)
        .await
        .expect("arm attempt record trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    }

    async fn disarm_attempt_record_failure(pool: &PgPool) {
        sqlx::query(
            "DROP TRIGGER reject_attempt_record_trigger
             ON auto_queue_run_cleanup_tasks",
        )
        .execute(pool)
        .await
        .expect("disarm attempt record trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
    }

    /// `next_attempt_at` in the future means the failure armed a real backoff.
    async fn task_is_backing_off(pool: &PgPool, id: i64) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT next_attempt_at > NOW() FROM auto_queue_run_cleanup_tasks WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("load cleanup backoff state") // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
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
    ///
    /// **Known limitation — read before trusting this test.** The fast-forward
    /// below reads `MAX_CLEANUP_ATTEMPTS` from the constant it is meant to
    /// protect, so the assertions hold for *any* value of it. This test proves
    /// "there is a cap and crossing it dead-letters the row"; it does not pin
    /// where the cap is. The `assert_eq!` on the literals in
    /// `attempt_cap_dead_letter_is_counted_and_surfaced_on_health_pg` is what
    /// pins the value and the wall-clock budget that follows from it.
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

    // ---------------------------------------------------------------------
    // #5142 round 3 — the two silent losses round 2 left behind.
    // ---------------------------------------------------------------------

    /// **The attempt-cap dead-letter must be observable.**
    ///
    /// Round 2 dead-lettered a row that burned through `MAX_CLEANUP_ATTEMPTS`
    /// and then said nothing about it: `stats.dead_lettered` only ever counted
    /// undecodable payloads, no query anywhere read `dead_lettered_at`, and the
    /// row simply stopped matching the drain predicate. The run's slot token and
    /// residual provider session id were stranded with no signal at all — the
    /// #5068 shape (a recovery owner exists, but some rows are never recovered
    /// and nothing says so).
    ///
    /// This test fixes both halves and keeps them apart:
    ///
    /// - the **counter** is a transition count — it fires on the sweep that
    ///   parks the row and never again;
    /// - the **health gauge** is a standing backlog — it keeps reporting the
    ///   parked row until an operator deals with it.
    ///
    /// A regression in either one alone still fails here.
    #[tokio::test]
    async fn attempt_cap_dead_letter_is_counted_and_surfaced_on_health_pg() {
        // Pin the cap itself. `repeatedly_failing_task_dead_letters_at_the_
        // attempt_cap_pg` reads the constant it is asserting on, so it holds for
        // any value; these literals are the only place the number and the wall
        // clock that follows from it are actually fixed.
        assert_eq!(
            MAX_CLEANUP_ATTEMPTS, 10,
            "the attempt cap is part of the operational contract, not a tunable"
        );
        assert_eq!(
            MAX_BACKOFF_SECONDS, 256,
            "the clamp must equal the ceiling of POWER(2, LEAST(attempts + 1, 8)); \
             a larger value is unreachable and documents a bound the code cannot hit"
        );
        let total_backoff: i64 = (1..MAX_CLEANUP_ATTEMPTS)
            .map(|attempt| MAX_BACKOFF_SECONDS.min(1i64 << attempt.min(8)))
            .sum();
        assert_eq!(
            total_backoff, 766,
            "2+4+8+16+32+64+128+256+256 seconds of backoff before the cap fires. \
             With the 30s policy tick rounding each delay up, a cleanup step that \
             keeps failing for roughly 13-17 minutes is parked permanently"
        );

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, _dispatch_id) = seed_run_holding_slot(&pool, "deadletter").await;

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        // Nothing is parked yet, so the gauge must read zero — otherwise a
        // non-zero reading below would prove nothing.
        let before = crate::services::health_diagnostics::load_auto_queue_cleanup_backlog_pg(
            crate::services::hang_forensics::ProbedPool::wrap(Some(&pool))
                .expect("a Some pool wraps"), // agentdesk-audit: allow-unwrap — test setup
        )
        .await
        .expect("load cleanup backlog"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(before.pending, 1);
        assert_eq!(before.dead_lettered, 0);

        // Spend everything but the last attempt, then make that last one fail.
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

        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay onto the attempt cap"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        sqlx::query("ALTER TABLE sessions_hidden RENAME TO sessions")
            .execute(&pool)
            .await
            .expect("restore sessions table"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        // Axis 1 — the counter. Round 2 reported `drained: 1, completed: 0,
        // dead_lettered: 0` here, which reads as an ordinary retry and is a lie:
        // this row will never be retried again.
        assert_eq!(stats.drained, 1);
        assert_eq!(stats.completed, 0);
        assert_eq!(
            stats.dead_lettered, 1,
            "burning the attempt cap must be counted, not merely absent from \
             `completed` — this is the only moment anything can report it"
        );
        assert!(
            stats.touched(),
            "a sweep that parked a row is not a no-op sweep"
        );
        assert!(
            task_retry_state(&pool, cancelled.cleanup_task_id).await.2,
            "precondition for the gauge: the row really is parked"
        );

        // Axis 2 — the standing backlog on `/api/health`. The slot token and the
        // provider session id are still on disk and nothing will ever retry
        // them, so the number has to survive past the sweep that produced it.
        let after = crate::services::health_diagnostics::load_auto_queue_cleanup_backlog_pg(
            crate::services::hang_forensics::ProbedPool::wrap(Some(&pool))
                .expect("a Some pool wraps"), // agentdesk-audit: allow-unwrap — test setup
        )
        .await
        .expect("load cleanup backlog"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(
            after.dead_lettered, 1,
            "a parked cleanup row must be visible to an operator who never saw \
             the sweep's log line"
        );
        assert_eq!(
            after.pending, 0,
            "and it must not be counted as live work that is still retrying"
        );
        assert_eq!(
            slot_assignment(&pool).await,
            Some(run_id.clone()),
            "the stranded state the gauge is reporting: the slot token is still held"
        );

        // The two axes are genuinely different measurements. A later sweep finds
        // nothing to do (no new transition) while the backlog is unchanged.
        wind_back_next_attempt(&pool).await;
        let quiet = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay after the row was parked"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(
            quiet,
            RunCleanupReplayStats::default(),
            "the counter counts the transition, so it must not re-count a row \
             that was already parked"
        );
        assert_eq!(
            crate::services::health_diagnostics::load_auto_queue_cleanup_backlog_pg(
                crate::services::hang_forensics::ProbedPool::wrap(Some(&pool))
                    .expect("a Some pool wraps"), // agentdesk-audit: allow-unwrap — test setup
            )
            .await
            .expect("load cleanup backlog") // agentdesk-audit: allow-unwrap — production entrypoint assertion
            .dead_lettered,
            1,
            "the gauge is a standing backlog, so it must survive the sweep that \
             found nothing to do"
        );
    }

    /// **The `!task.emitted` guard is what makes "never double-counting" true.**
    ///
    /// The module header promises the observability emit is at-most-once. The
    /// only thing enforcing that is `!task.emitted` in step 1, and no test
    /// observed it: deleting the guard left the whole suite green.
    ///
    /// The emit itself is fire-and-forget into an in-process channel, so it
    /// cannot be observed directly. What can be observed is the statement that
    /// now immediately precedes it — the `emitted = TRUE` mark, which round 3
    /// moved *ahead* of `emit()` precisely so that "about to emit" has a durable,
    /// interceptable footprint. `arm_emit_remark_trap` rejects exactly that
    /// statement when the row is already marked, so a drain that tries to emit
    /// again fails loudly instead of silently duplicating an event.
    ///
    /// With the guard: the drain never touches the emit path and converges.
    /// Without it: the mark re-fires, the trap raises, and the drain reports
    /// `completed: false` with the slot still held.
    #[tokio::test]
    async fn an_already_emitted_task_is_never_re_emitted_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, dispatch_id) = seed_run_holding_slot(&pool, "reemit").await;

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        // Without a pending emit the guard is vacuous and this test would pass
        // for the wrong reason.
        let pending_emits = sqlx::query_scalar::<_, Option<i32>>(
            "SELECT jsonb_array_length(pending_emits)
             FROM auto_queue_run_cleanup_tasks WHERE id = $1",
        )
        .bind(cancelled.cleanup_task_id)
        .fetch_one(&pool)
        .await
        .expect("count pending emits"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
        assert_eq!(
            pending_emits,
            Some(1),
            "the cancel must have persisted the emit it owes, or the guard under \
             test is never reached"
        );

        // Model the state a crashed drain leaves behind after step 1 committed:
        // the events are out and durably marked, and everything after them is
        // still owed.
        sqlx::query("UPDATE auto_queue_run_cleanup_tasks SET emitted = TRUE WHERE id = $1")
            .bind(cancelled.cleanup_task_id)
            .execute(&pool)
            .await
            .expect("mark the emits as already fired"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        let emits_before = emit_probe::emit_count(&dispatch_id);
        arm_emit_remark_trap(&pool).await;
        let outcome = drain_run_cleanup_task_by_id_pg(None, &pool, cancelled.cleanup_task_id).await;
        disarm_emit_remark_trap(&pool).await;

        assert_eq!(
            emit_probe::emit_count(&dispatch_id),
            emits_before,
            "a drain that finds the row already marked must fire no event at all"
        );
        assert!(
            outcome.completed,
            "the replay must skip step 1 entirely and finish the remaining steps; \
             re-marking the row means it was about to fire the same observability \
             events a second time: {outcome:?}"
        );
        assert!(
            !outcome.dead_lettered,
            "a clean drain must not park the row: {outcome:?}"
        );
        assert!(
            outcome.slot_cleanup.warnings.is_empty(),
            "no step may have degraded into a warning: {outcome:?}"
        );
        assert_eq!(
            slot_assignment(&pool).await,
            None,
            "and the steps the crashed drain still owed must have run"
        );
        assert_eq!(provider_session_ids(&pool).await, vec![None]);
        assert_eq!(
            pending_run_cleanup_task_count_pg(&pool)
                .await
                .expect("count cleanup tasks"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
            0
        );
    }

    /// **Round 3, P2-2 companion — and the test that pins step 1's ordering.**
    ///
    /// Round 2 pushed a warning and carried on. That mattered because the mark
    /// failing and a later step failing are highly correlated (both mean
    /// PostgreSQL is unwell): the row then survived with `emitted = FALSE` while
    /// the events had already been sent, and the next replay sent them again.
    /// The fix is the ordering — mark first, emit second — plus this early
    /// return, which makes a failed mark cost nothing at all because no event
    /// left the process.
    ///
    /// **The name used to be `..._before_any_side_effect_pg`, and that was a
    /// lie.** The emit is fire-and-forget into an in-process channel, so nothing
    /// in this test counted it as a side effect: moving the emit loop back in
    /// front of the mark left this test — and the whole module — green (#5142 r3
    /// review, mutation ⓟ). The `#[cfg(test)]` probe on
    /// `CancelTransitionMeta::emit` is what makes the emit observable, and the
    /// name now enumerates exactly the two things asserted below rather than
    /// claiming everything.
    ///
    /// **The probe sits on the emit, not on a wrapper.** Round 4 recorded inside
    /// a `fire_pending_emit` helper, so `fires_no_emit` was true only of emits
    /// routed through that helper: writing a bare `meta.emit()` in front of the
    /// mark fired the real events and this test still passed (#5142 r5, mutation
    /// S1). Counting at the boundary makes the name a statement about the code
    /// rather than about one call site.
    #[tokio::test]
    async fn a_failed_emit_mark_fires_no_emit_and_releases_no_slot_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, dispatch_id) = seed_run_holding_slot(&pool, "markfail").await;
        let emits_before = emit_probe::emit_count(&dispatch_id);

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        // Reject the mark itself: any UPDATE that flips `emitted` to TRUE.
        sqlx::query(
            "CREATE OR REPLACE FUNCTION reject_emit_mark()
             RETURNS trigger AS $$
             BEGIN
                 IF NEW.emitted AND NOT OLD.emitted THEN
                     RAISE EXCEPTION 'injected emitted-mark failure';
                 END IF;
                 RETURN NEW;
             END;
             $$ LANGUAGE plpgsql",
        )
        .execute(&pool)
        .await
        .expect("define emit mark trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        sqlx::query(
            "CREATE TRIGGER reject_emit_mark_trigger
             BEFORE UPDATE ON auto_queue_run_cleanup_tasks
             FOR EACH ROW EXECUTE FUNCTION reject_emit_mark()",
        )
        .execute(&pool)
        .await
        .expect("arm emit mark trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        let outcome = drain_run_cleanup_task_by_id_pg(None, &pool, cancelled.cleanup_task_id).await;
        assert!(
            !outcome.completed,
            "a drain that could not durably record its emits must not report success"
        );

        // **The ordering discriminator.** The mark is the statement that says
        // "these events are about to go out"; it failed, so nothing may have
        // gone out. Emit-then-mark passes every other assertion in this test and
        // fails only here.
        assert_eq!(
            emit_probe::emit_count(&dispatch_id),
            emits_before,
            "the emit must happen AFTER the durable mark: a failed mark means no \
             event may have left the process, otherwise the next replay re-marks \
             and re-sends the same observability rows"
        );
        assert!(
            outcome
                .slot_cleanup
                .warnings
                .iter()
                .any(|warning| warning.contains("mark auto-queue cleanup emits")),
            "the cause must be surfaced: {:?}",
            outcome.slot_cleanup.warnings
        );

        // Nothing downstream ran, and the failure is a real recorded attempt
        // rather than a warning string that disappears with the process.
        assert_eq!(
            slot_assignment(&pool).await,
            Some(run_id.clone()),
            "the drain must stop before releasing the slot"
        );
        let (attempts, last_error, dead_lettered) =
            task_retry_state(&pool, cancelled.cleanup_task_id).await;
        assert_eq!(
            attempts, 1,
            "the failed mark must be recorded as an attempt"
        );
        assert!(last_error.is_some_and(|error| error.contains("mark auto-queue cleanup emits")));
        assert!(!dead_lettered, "one failure is far from the cap");

        sqlx::query("DROP TRIGGER reject_emit_mark_trigger ON auto_queue_run_cleanup_tasks")
            .execute(&pool)
            .await
            .expect("disarm emit mark trap"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        // And the retry converges, emitting exactly once. `+ 1` rather than
        // `>= 1`: emit-then-mark would have emitted during the failed attempt as
        // well, so the total would be two.
        wind_back_next_attempt(&pool).await;
        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay after the mark trap was removed"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.dead_lettered, 0);
        assert_eq!(stats.unrecorded_failures, 0);
        assert_eq!(
            emit_probe::emit_count(&dispatch_id),
            emits_before + 1,
            "the owed event must fire exactly once across the failed attempt and \
             the successful retry"
        );
        assert_eq!(slot_assignment(&pool).await, None);
        assert_eq!(provider_session_ids(&pool).await, vec![None]);
    }

    // ---------------------------------------------------------------------
    // #5142 round 4 — the bookkeeping writes that used to swallow their own
    // failures.
    // ---------------------------------------------------------------------

    /// **#5142 r3 P3-3, now under test (mutation ⓢ).** A task whose retirement
    /// `DELETE` fails must record an attempt like every other failure path.
    ///
    /// Without it the row is the one shape that neither converges nor ever
    /// dead-letters: every drain step succeeds, the `DELETE` fails, `attempts`
    /// stays where it was, no backoff is armed, and the whole drain re-runs at
    /// lease-expiry rate forever. r3 fixed that and no test observed the fix —
    /// reverting it left the module green, because nothing anywhere injected a
    /// `DELETE` failure.
    ///
    /// The trap is armed only around the retirement write, so this test also
    /// proves *where* the failure happened: the slot really was released and the
    /// provider session really was cleared before the `DELETE` was reached.
    #[tokio::test]
    async fn a_failed_retirement_delete_records_an_attempt_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, _dispatch_id) = seed_run_holding_slot(&pool, "delfail").await;

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        arm_cleanup_task_delete_failure(&pool).await;
        let outcome = drain_run_cleanup_task_by_id_pg(None, &pool, cancelled.cleanup_task_id).await;

        assert!(
            !outcome.completed,
            "a task that could not be retired is still owed: {outcome:?}"
        );
        assert!(
            !outcome.attempt_unrecorded,
            "the attempt bookkeeping itself was working here: {outcome:?}"
        );
        // Everything before the DELETE ran, which is what makes this a pure
        // retirement-write failure rather than an early abort.
        assert_eq!(slot_assignment(&pool).await, None);
        assert_eq!(provider_session_ids(&pool).await, vec![None]);

        let (attempts, last_error, dead_lettered) =
            task_retry_state(&pool, cancelled.cleanup_task_id).await;
        assert_eq!(
            attempts, 1,
            "an undeletable row must burn an attempt — otherwise it can never \
             reach the terminal cap and re-runs the whole drain forever"
        );
        assert!(
            last_error
                .is_some_and(|error| error.contains("delete finished auto-queue cleanup task")),
            "the retry record must name the retirement write as the cause"
        );
        assert!(!dead_lettered, "one failure is far from the cap");
        assert!(
            task_is_backing_off(&pool, cancelled.cleanup_task_id).await,
            "and the attempt must arm the same exponential backoff as every \
             other failure, instead of spinning at lease-expiry rate"
        );

        // Once the write works again the row retires and stops being replayed.
        disarm_cleanup_task_delete_failure(&pool).await;
        wind_back_next_attempt(&pool).await;
        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay after the delete trap was removed"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.unrecorded_failures, 0);
        assert_eq!(
            pending_run_cleanup_task_count_pg(&pool)
                .await
                .expect("count cleanup tasks"), // agentdesk-audit: allow-unwrap — test-only PostgreSQL assertion
            0
        );
    }

    /// **The second bookkeeping write.** `dead_letter_task_pg` used to swallow
    /// its own UPDATE failure, so the sweep reported a dead-letter that never
    /// happened while the poison row stayed drainable and was re-decoded on every
    /// lease expiry.
    ///
    /// Two things are pinned here and they are independent: the sweep must not
    /// *claim* a transition that did not land, and the row must still converge on
    /// a terminal state through the ordinary attempt bookkeeping.
    #[tokio::test]
    async fn a_poison_row_whose_park_write_fails_is_not_reported_as_parked_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let poison_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO auto_queue_run_cleanup_tasks
                (run_ids, dispatch_ids, released_slots, pending_emits)
             VALUES ('{}', '{}', '[]'::jsonb, '\"not-an-array\"'::jsonb)
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("seed undecodable cleanup task"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        arm_dead_letter_write_failure(&pool).await;
        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay while the park write fails"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        assert_eq!(
            stats.dead_lettered, 0,
            "the park did not land, so no transition may be reported: {stats:?}"
        );
        assert_eq!(
            stats.unrecorded_failures, 1,
            "the decision the sweep made and could not write must be counted: {stats:?}"
        );
        assert!(
            stats.touched(),
            "a sweep that failed to park a row is not a no-op"
        );
        let (attempts, _last_error, parked) = task_retry_state(&pool, poison_id).await;
        assert!(!parked, "precondition: the trap really did block the park");
        assert_eq!(
            attempts, 1,
            "the fallback must still burn an attempt, so the row backs off and \
             eventually reaches the terminal cap instead of being re-decoded at \
             lease-expiry rate forever"
        );

        // With the write restored the row parks for good and is counted once.
        disarm_dead_letter_write_failure(&pool).await;
        wind_back_next_attempt(&pool).await;
        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay after the park write recovered"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(stats.dead_lettered, 1);
        assert_eq!(stats.unrecorded_failures, 0);
        assert!(task_retry_state(&pool, poison_id).await.2);
    }

    /// **The third bookkeeping write.** `record_task_failure_pg` used to return a
    /// bare `false` when its own UPDATE failed, which is indistinguishable from
    /// "the attempt was recorded and the row is not at the cap yet". The two have
    /// opposite convergence properties: an unrecorded attempt leaves `attempts`
    /// untouched, so the row never approaches the cap at all.
    #[tokio::test]
    async fn an_unrecordable_attempt_is_reported_instead_of_looking_like_a_retry_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, _dispatch_id) = seed_run_holding_slot(&pool, "noattempt").await;

        let cancelled =
            cancel_live_dispatches_for_runs_pg(&pool, &[run_id.clone()], "auto_queue_cancel")
                .await
                .expect("cancel run-owned dispatches"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        // Break a drain step, then break the bookkeeping that would record it.
        sqlx::query("ALTER TABLE sessions RENAME TO sessions_hidden")
            .execute(&pool)
            .await
            .expect("hide sessions table"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
        arm_attempt_record_failure(&pool).await;

        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay while the attempt bookkeeping fails"); // agentdesk-audit: allow-unwrap — production entrypoint assertion

        disarm_attempt_record_failure(&pool).await;
        sqlx::query("ALTER TABLE sessions_hidden RENAME TO sessions")
            .execute(&pool)
            .await
            .expect("restore sessions table"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        assert_eq!(stats.drained, 1);
        assert_eq!(stats.completed, 0);
        assert_eq!(stats.dead_lettered, 0);
        assert_eq!(
            stats.unrecorded_failures, 1,
            "a failure whose bookkeeping did not land must be reported as such, \
             not as an ordinary backing-off retry: {stats:?}"
        );

        let (attempts, _last_error, parked) =
            task_retry_state(&pool, cancelled.cleanup_task_id).await;
        assert_eq!(
            attempts, 0,
            "precondition for the counter: nothing about the attempt reached disk"
        );
        assert!(!parked);

        // The claim is released by the same UPDATE that records the attempt, so
        // a failure there also strands the claim. That is the lease's job — model
        // its expiry rather than pretending the row is immediately drainable.
        assert!(
            !replay_pending_run_cleanup_tasks_pg(None, &pool)
                .await
                .expect("replay while the stranded claim is still inside its lease") // agentdesk-audit: allow-unwrap — production entrypoint assertion
                .touched(),
            "the stranded claim must hold until the lease expires"
        );
        sqlx::query(
            "UPDATE auto_queue_run_cleanup_tasks
             SET claimed_at = NOW() - ($1::BIGINT * INTERVAL '1 second')",
        )
        .bind(CLAIM_LEASE_SECONDS + 60)
        .execute(&pool)
        .await
        .expect("expire the stranded claim"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture

        // The row is not lost: the next sweep re-derives the same work and, with
        // both injections gone, converges.
        let stats = replay_pending_run_cleanup_tasks_pg(None, &pool)
            .await
            .expect("replay after both injections were removed"); // agentdesk-audit: allow-unwrap — production entrypoint assertion
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.unrecorded_failures, 0);
        assert_eq!(slot_assignment(&pool).await, None);
        assert_eq!(provider_session_ids(&pool).await, vec![None]);
    }

    /// #5357: NULL generations never enter replay. The card rollback and the
    /// cancel commit together, while the outbox contains only replay-safe Some
    /// generations. The delete failure keeps the drained row observable so the
    /// JSONB enrollment boundary can be asserted after the inline drain.
    #[tokio::test]
    async fn cancel_rolls_back_null_generation_in_tx_without_enrolling_it_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, dispatch_id) = seed_run_holding_slot(&pool, "null-in-tx").await;
        let card_id = "card-cleanup-null-in-tx";

        sqlx::query(
            "CREATE FUNCTION keep_cleanup_row_for_null_enrollment_test()
             RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
                 RAISE EXCEPTION 'keep cleanup row for enrollment assertion';
             END
             $$",
        )
        .execute(&pool)
        .await
        .expect("install cleanup-row retention function");
        sqlx::query(
            "CREATE TRIGGER keep_cleanup_row_for_null_enrollment_test
             BEFORE DELETE ON auto_queue_run_cleanup_tasks
             FOR EACH ROW EXECUTE FUNCTION keep_cleanup_row_for_null_enrollment_test()",
        )
        .execute(&pool)
        .await
        .expect("install cleanup-row retention trigger");

        cancel_selected_runs_with_pg(None, &pool, std::slice::from_ref(&run_id), "test_cancel")
            .await
            .expect("cancel NULL-generation card run");

        let card: (String, Option<String>) =
            sqlx::query_as("SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = $1")
                .bind(card_id)
                .fetch_one(&pool)
                .await
                .expect("load synchronously rolled-back card");
        assert_eq!(card, ("ready".to_string(), None));

        let enrolled: serde_json::Value = sqlx::query_scalar(
            "SELECT card_rollback_tasks
             FROM auto_queue_run_cleanup_tasks
             WHERE run_ids @> ARRAY[$1]::TEXT[]",
        )
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .expect("load retained cleanup enrollment");
        assert_eq!(enrolled, serde_json::json!([]));

        let states: (String, String, String) = sqlx::query_as(
            "SELECT
                 (SELECT status FROM auto_queue_runs WHERE id = $1),
                 (SELECT status FROM auto_queue_entries WHERE run_id = $1),
                 (SELECT status FROM task_dispatches WHERE id = $2)",
        )
        .bind(&run_id)
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("load committed cancel states");
        assert_eq!(
            states,
            (
                "cancelled".to_string(),
                "skipped".to_string(),
                "cancelled".to_string()
            )
        );
    }

    /// #5357: generation enrollment and row locking must be one operation.
    /// The sentinel lock pauses cancellation while it rolls back the first
    /// candidate. At that point the ordered enrollment query must already hold
    /// the second candidate, forcing an operator transition to serialize after
    /// cancellation instead of letting cancellation erase that new lifecycle.
    #[tokio::test]
    async fn cancel_null_generation_snapshot_serializes_operator_transition_pg() {
        use std::time::Duration;
        use tokio::time::{Instant, sleep};

        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        let (run_id, _dispatch_id) = seed_run_holding_slot(&pool, "null-aba-a").await;
        let first_card_id = "card-cleanup-null-aba-a";
        let target_card_id = "card-cleanup-null-aba-b";
        let target_dispatch_id = "dispatch-cleanup-null-aba-b";
        let sentinel_card_id = "card-cleanup-null-aba-sentinel";

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES
                 ($1, 'NULL ABA target', 'in_progress', NULL),
                 ($2, 'NULL ABA sentinel', 'ready', 'agent-cleanup')",
        )
        .bind(target_card_id)
        .bind(sentinel_card_id)
        .execute(&pool)
        .await
        .expect("seed NULL ABA target and sentinel cards");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
             VALUES ($1, $2, 'agent-cleanup', 'implementation', 'dispatched', 'NULL ABA target')",
        )
        .bind(target_dispatch_id)
        .bind(target_card_id)
        .execute(&pool)
        .await
        .expect("seed NULL ABA target dispatch");
        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id)
             VALUES
                ('entry-cleanup-null-aba-b', $1, $2, 'agent-cleanup', 'dispatched', $3)",
        )
        .bind(&run_id)
        .bind(target_card_id)
        .bind(target_dispatch_id)
        .execute(&pool)
        .await
        .expect("seed NULL ABA target entry");

        sqlx::query(
            "CREATE FUNCTION block_first_null_aba_rollback_on_sentinel()
             RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
                 IF OLD.id = 'card-cleanup-null-aba-a'
                    AND OLD.status IN ('requested', 'in_progress')
                    AND NEW.status = 'ready'
                 THEN
                     PERFORM id FROM kanban_cards
                     WHERE id = 'card-cleanup-null-aba-sentinel'
                     FOR UPDATE;
                 END IF;
                 RETURN NEW;
             END
             $$",
        )
        .execute(&pool)
        .await
        .expect("install NULL ABA blocking function");
        sqlx::query(
            "CREATE TRIGGER block_first_null_aba_rollback_on_sentinel
             BEFORE UPDATE OF status ON kanban_cards
             FOR EACH ROW EXECUTE FUNCTION block_first_null_aba_rollback_on_sentinel()",
        )
        .execute(&pool)
        .await
        .expect("install NULL ABA blocking trigger");

        let mut blocker = pool.begin().await.expect("begin NULL ABA blocker");
        sqlx::query("SELECT id FROM kanban_cards WHERE id = $1 FOR UPDATE")
            .bind(sentinel_card_id)
            .fetch_one(&mut *blocker)
            .await
            .expect("lock NULL ABA sentinel card");
        let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *blocker)
            .await
            .expect("load NULL ABA blocker backend pid");

        let cancel_pool = pool.clone();
        let cancel_run_id = run_id.clone();
        let cancel_task = tokio::spawn(async move {
            cancel_selected_runs_with_pg(
                None,
                &cancel_pool,
                &[cancel_run_id],
                "test_cancel_null_aba",
            )
            .await
        });

        let wait_deadline = Instant::now() + Duration::from_secs(10);
        let cancel_pid = loop {
            let waiting_pid: Option<i32> = sqlx::query_scalar(
                "SELECT pid
                 FROM pg_stat_activity
                 WHERE datname = current_database()
                   AND wait_event_type = 'Lock'
                   AND $1 = ANY(pg_blocking_pids(pid))
                 ORDER BY pid
                 LIMIT 1",
            )
            .bind(blocker_pid)
            .fetch_optional(&pool)
            .await
            .expect("observe cancellation waiting on NULL ABA sentinel");
            if let Some(pid) = waiting_pid {
                break pid;
            }
            assert!(
                Instant::now() < wait_deadline,
                "cancellation never waited on the sentinel card lock"
            );
            sleep(Duration::from_millis(10)).await;
        };

        let transition_pool = pool.clone();
        let engine = crate::engine::PolicyEngine::new_with_pg(
            &crate::config::Config::default(),
            Some(pool.clone()),
        )
        .expect("create NULL ABA transition engine");
        let transition_engine = engine.clone();
        let transition_task = tokio::spawn(async move {
            crate::kanban::transition_status_with_opts_pg_only(
                &transition_pool,
                &transition_engine,
                target_card_id,
                "requested",
                "test_force_transition_null_aba",
                crate::engine::transition::ForceIntent::OperatorOverride,
            )
            .await
        });

        let transition_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let waiting_on_cancel: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                     SELECT 1
                     FROM pg_stat_activity
                     WHERE datname = current_database()
                       AND wait_event_type = 'Lock'
                       AND $1 = ANY(pg_blocking_pids(pid))
                 )",
            )
            .bind(cancel_pid)
            .fetch_one(&pool)
            .await
            .expect("observe operator transition serialization");
            if waiting_on_cancel || transition_task.is_finished() {
                break;
            }
            assert!(
                Instant::now() < transition_deadline,
                "operator transition neither committed nor waited on cancellation"
            );
            sleep(Duration::from_millis(10)).await;
        }

        blocker
            .commit()
            .await
            .expect("release NULL ABA sentinel card");
        cancel_task
            .await
            .expect("join NULL ABA cancellation")
            .expect("cancel NULL ABA run");
        transition_task
            .await
            .expect("join NULL ABA operator transition")
            .expect("force target card into a new requested lifecycle");

        let target_card: (String, Option<String>) =
            sqlx::query_as("SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = $1")
                .bind(target_card_id)
                .fetch_one(&pool)
                .await
                .expect("load NULL ABA target after serialization");
        assert_eq!(
            target_card,
            ("requested".to_string(), None),
            "the authorized requested/NULL lifecycle must survive cancellation"
        );

        let first_card: (String, Option<String>) =
            sqlx::query_as("SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = $1")
                .bind(first_card_id)
                .fetch_one(&pool)
                .await
                .expect("load first NULL ABA cancellation card");
        assert_eq!(first_card, ("ready".to_string(), None));
    }

    /// The trigger fails specifically when the shared in-transaction rollback
    /// changes the NULL-generation card to ready. The resulting error must
    /// abort the enclosing cancel transaction, not leave any terminal run,
    /// entry, dispatch, or partial card state behind.
    #[tokio::test]
    async fn null_generation_rollback_failure_aborts_cancel_transaction_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let (run_id, dispatch_id) = seed_run_holding_slot(&pool, "null-atomic").await;
        let card_id = "card-cleanup-null-atomic";

        sqlx::query(
            "CREATE FUNCTION fail_null_card_in_tx_rollback_test()
             RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
                 IF OLD.status IN ('requested', 'in_progress') AND NEW.status = 'ready' THEN
                     RAISE EXCEPTION 'forced in-tx card rollback failure';
                 END IF;
                 RETURN NEW;
             END
             $$",
        )
        .execute(&pool)
        .await
        .expect("install in-tx rollback failure function");
        sqlx::query(
            "CREATE TRIGGER fail_null_card_in_tx_rollback_test
             BEFORE UPDATE OF status ON kanban_cards
             FOR EACH ROW EXECUTE FUNCTION fail_null_card_in_tx_rollback_test()",
        )
        .execute(&pool)
        .await
        .expect("install in-tx rollback failure trigger");

        let error = cancel_selected_runs_with_pg(
            None,
            &pool,
            std::slice::from_ref(&run_id),
            "test_cancel_atomicity",
        )
        .await
        .expect_err("in-tx card rollback failure must abort cancel");
        assert!(error.contains("forced in-tx card rollback failure"));

        let states: (String, String, String, String, Option<String>) = sqlx::query_as(
            "SELECT r.status, e.status, d.status, c.status, c.latest_dispatch_id
             FROM auto_queue_runs r
             JOIN auto_queue_entries e ON e.run_id = r.id
             JOIN task_dispatches d ON d.id = e.dispatch_id
             JOIN kanban_cards c ON c.id = e.kanban_card_id
             WHERE r.id = $1 AND d.id = $2 AND c.id = $3",
        )
        .bind(&run_id)
        .bind(&dispatch_id)
        .bind(card_id)
        .fetch_one(&pool)
        .await
        .expect("load states after aborted cancel");
        assert_eq!(
            states,
            (
                "active".to_string(),
                "dispatched".to_string(),
                "dispatched".to_string(),
                "in_progress".to_string(),
                None
            )
        );
        let task_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM auto_queue_run_cleanup_tasks")
                .fetch_one(&pool)
                .await
                .expect("count outbox rows after aborted cancel");
        assert_eq!(task_count, 0);
    }

    #[tokio::test]
    async fn end_run_does_not_change_card_state_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        // Seed: create agent, card (in_progress), run, dispatch, entry
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-end', 'End Agent', 'claude', '123')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(&pool)
        .await
        .expect("seed end agent");

        let card_id = "card-end-test";
        let run_id = "run-end-test";
        let dispatch_id = "dispatch-end-test";
        let entry_id = "entry-end-test";

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, review_round, review_notes)
             VALUES ($1, 'End Card', 'in_progress', 'agent-end', 1, 'some notes')",
        )
        .bind(card_id)
        .execute(&pool)
        .await
        .expect("seed end card");

        sqlx::query(
            "INSERT INTO auto_queue_runs (id, agent_id, status)
             VALUES ($1, 'agent-end', 'active')",
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .expect("seed end run");

        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
             VALUES ($1, $2, 'agent-end', 'implementation', 'dispatched', 'End Dispatch')",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .execute(&pool)
        .await
        .expect("seed end dispatch");

        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id)
             VALUES ($1, $2, $3, 'agent-end', 'dispatched', $4)",
        )
        .bind(entry_id)
        .bind(run_id)
        .bind(card_id)
        .bind(dispatch_id)
        .execute(&pool)
        .await
        .expect("seed end entry");

        // Before end: verify card state
        let (initial_status, initial_review_round, initial_review_notes): (
            String,
            i64,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT status, review_round, review_notes FROM kanban_cards WHERE id = $1",
        )
        .bind(card_id)
        .fetch_one(&pool)
        .await
        .expect("load initial card state");

        assert_eq!(initial_status, "in_progress");
        assert_eq!(initial_review_round, 1);
        assert_eq!(initial_review_notes, Some("some notes".to_string()));

        // P1-A: end_run_with_pg should NOT roll back cards
        end_run_with_pg(None, &pool, run_id).await.expect("end run");

        // After end: verify card state is UNCHANGED
        let (final_status, final_review_round, final_review_notes): (String, i64, Option<String>) =
            sqlx::query_as(
                "SELECT status, review_round, review_notes FROM kanban_cards WHERE id = $1",
            )
            .bind(card_id)
            .fetch_one(&pool)
            .await
            .expect("load final card state");

        assert_eq!(
            final_status, initial_status,
            "P1-A: end_run must NOT change card status"
        );
        assert_eq!(
            final_review_round, initial_review_round,
            "P1-A: end_run must NOT change card review_round"
        );
        assert_eq!(
            final_review_notes, initial_review_notes,
            "P1-A: end_run must NOT change card review_notes"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// #5357: NULL in an outbox row is corruption/tampering because enrollment
    /// excludes it. The drain skips only that card and records a warning while
    /// continuing a replay-safe Some sibling from the same row.
    #[tokio::test]
    async fn tampered_null_generation_skips_card_but_rolls_back_some_sibling_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let null_card_id = "card-tampered-null-generation";
        let some_card_id = "card-valid-some-generation";
        let some_dispatch_id = "dispatch-valid-some-generation";
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-generation-partition', 'Generation Partition Agent', 'claude', '5357')",
        )
        .execute(&pool)
        .await
        .expect("seed generation-partition agent");
        sqlx::query(
            "INSERT INTO kanban_cards
                (id, title, status, assigned_agent_id, latest_dispatch_id)
             VALUES ($1, 'Tampered NULL Card', 'requested', 'agent-generation-partition', NULL),
                    ($2, 'Valid Some Card', 'in_progress', 'agent-generation-partition', $3)",
        )
        .bind(null_card_id)
        .bind(some_card_id)
        .bind(some_dispatch_id)
        .execute(&pool)
        .await
        .expect("seed mixed-generation cards");
        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
             VALUES ($1, $2, 'agent-generation-partition', 'implementation',
                     'cancelled', 'Valid Some Dispatch')",
        )
        .bind(some_dispatch_id)
        .bind(some_card_id)
        .execute(&pool)
        .await
        .expect("seed terminal Some-generation dispatch");

        let task_id = {
            let mut tx = pool.begin().await.expect("begin mixed-generation enqueue");
            let task_id = super::super::enqueue_run_cleanup_task_on_tx(
                &mut tx,
                &[],
                &[],
                &[],
                &[],
                &[(some_card_id.to_string(), Some(some_dispatch_id.to_string()))],
                Some("test_tampered_null_generation"),
            )
            .await
            .expect("enqueue valid Some-generation rollback");
            tx.commit().await.expect("commit mixed-generation enqueue");
            task_id
        };

        let tampered = serde_json::json!([
            {"card_id": null_card_id, "dispatch_id": null},
            {"card_id": some_card_id, "dispatch_id": some_dispatch_id}
        ]);
        sqlx::query(
            "UPDATE auto_queue_run_cleanup_tasks SET card_rollback_tasks = $1 WHERE id = $2",
        )
        .bind(tampered)
        .bind(task_id)
        .execute(&pool)
        .await
        .expect("inject forbidden NULL-generation rollback");

        let outcome = drain_run_cleanup_task_by_id_pg(None, &pool, task_id).await;
        assert!(outcome.completed, "mixed row must complete");
        assert!(
            outcome
                .slot_cleanup
                .warnings
                .iter()
                .any(|warning| warning.contains("forbidden NULL dispatch_id generation")),
            "tampered NULL card must use the existing warning surface"
        );

        let null_card: (String, Option<String>) =
            sqlx::query_as("SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = $1")
                .bind(null_card_id)
                .fetch_one(&pool)
                .await
                .expect("load skipped NULL-generation card");
        assert_eq!(null_card, ("requested".to_string(), None));

        let some_card: (String, Option<String>) =
            sqlx::query_as("SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = $1")
                .bind(some_card_id)
                .fetch_one(&pool)
                .await
                .expect("load rolled-back Some-generation card");
        assert_eq!(some_card, ("ready".to_string(), None));

        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM auto_queue_run_cleanup_tasks WHERE id = $1")
                .bind(task_id)
                .fetch_one(&pool)
                .await
                .expect("count mixed-generation cleanup row after drain");
        assert_eq!(remaining, 0, "completed mixed row must be deleted");
    }

    #[tokio::test]
    async fn generation_mismatch_skips_card_rollback_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        // Seed: agent, card, run, dispatches, entries
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-gen', 'Gen Agent', 'claude', '123')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(&pool)
        .await
        .expect("seed gen agent");

        let card_id = "card-gen-test";
        let run_id = "run-gen-test";
        let dispatch_id_old = "dispatch-gen-old";
        let dispatch_id_new = "dispatch-gen-new";
        let entry_id = "entry-gen-test";

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, review_round)
             VALUES ($1, 'Gen Card', 'in_progress', 'agent-gen', $2, 1)",
        )
        .bind(card_id)
        .bind(dispatch_id_old)
        .execute(&pool)
        .await
        .expect("seed gen card");

        // Both generations are terminal on purpose: that removes the
        // active-dispatch safety net and isolates the generation guard.
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, agent_id, status)
             VALUES ($1, 'agent-gen', 'active')",
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .expect("seed gen run");

        sqlx::query(
            "INSERT INTO task_dispatches
                (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
             VALUES ($1, $2, 'agent-gen', 'implementation', 'cancelled', 'Gen Old Dispatch'),
                    ($3, $2, 'agent-gen', 'implementation', 'cancelled', 'Gen New Dispatch')",
        )
        .bind(dispatch_id_old)
        .bind(card_id)
        .bind(dispatch_id_new)
        .execute(&pool)
        .await
        .expect("seed gen dispatches");

        sqlx::query(
            "INSERT INTO auto_queue_entries
                (id, run_id, kanban_card_id, agent_id, status, dispatch_id)
             VALUES ($1, $2, $3, 'agent-gen', 'dispatched', $4)",
        )
        .bind(entry_id)
        .bind(run_id)
        .bind(card_id)
        .bind(dispatch_id_old)
        .execute(&pool)
        .await
        .expect("seed gen entry");

        // Enqueue cleanup task with OLD dispatch_id generation marker
        let task_id = {
            let mut tx = pool
                .begin()
                .await
                .expect("begin cleanup enqueue transaction");

            let card_rollback_tasks =
                vec![(card_id.to_string(), Some(dispatch_id_old.to_string()))];
            let task_id = super::super::enqueue_run_cleanup_task_on_tx(
                &mut tx,
                &[run_id.to_string()],
                &[dispatch_id_old.to_string()],
                &[],
                &[],
                &card_rollback_tasks,
                Some("test_gen_mismatch"),
            )
            .await
            .expect("enqueue gen cleanup task");

            sqlx::query("UPDATE auto_queue_entries SET status = $1 WHERE id = $2")
                .bind("skipped")
                .bind(entry_id)
                .execute(&mut *tx)
                .await
                .expect("update gen entry to skipped");

            tx.commit().await.expect("commit gen cleanup enqueue");
            task_id
        };

        // Now update the card's latest_dispatch_id to NEW (simulating reassignment)
        sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = $2")
            .bind(dispatch_id_new)
            .bind(card_id)
            .execute(&pool)
            .await
            .expect("update card to new dispatch_id");

        // Before drain: card is still in_progress with review_round = 1
        let (status_before, review_round_before): (String, i64) =
            sqlx::query_as("SELECT status, review_round FROM kanban_cards WHERE id = $1")
                .bind(card_id)
                .fetch_one(&pool)
                .await
                .expect("load card state before drain");
        assert_eq!(status_before, "in_progress");
        assert_eq!(review_round_before, 1);

        // P1-A: Drain the cleanup task. Generation mismatch should cause skip.
        super::super::drain_run_cleanup_task_by_id_pg(None, &pool, task_id).await;

        // After drain: card should be UNCHANGED (skipped due to generation mismatch)
        let (status_after, review_round_after): (String, i64) =
            sqlx::query_as("SELECT status, review_round FROM kanban_cards WHERE id = $1")
                .bind(card_id)
                .fetch_one(&pool)
                .await
                .expect("load card state after drain");

        assert_eq!(
            status_after, status_before,
            "P1-A: generation mismatch must skip rollback; card status unchanged"
        );
        assert_eq!(
            review_round_after, review_round_before,
            "P1-A: generation mismatch must skip rollback; card review_round unchanged"
        );

        // After drain: cleanup task should be removed (drain completes despite skip)
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM auto_queue_run_cleanup_tasks WHERE id = $1")
                .bind(task_id)
                .fetch_one(&pool)
                .await
                .expect("count cleanup tasks after gen mismatch drain");
        assert_eq!(
            count, 0,
            "P1-A: cleanup task must be deleted after drain, generation mismatch is idempotent"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
