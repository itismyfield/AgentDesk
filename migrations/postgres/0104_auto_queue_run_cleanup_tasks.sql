-- #5142 P0: make the post-commit half of auto-queue run cancel/end durable.
--
-- `cancel_live_dispatches_for_runs_pg` and `terminalize_selected_runs_with_pg`
-- both commit the dispatch/run state change and only then run the remaining
-- cleanup (provider session clear, slot release, slot-thread clear, wait-queue
-- wake, observability emit). Before this table the fact that those steps still
-- owed work was held only in the process's stack, so a crash after the commit
-- left cancelled dispatches next to residual slot tokens and provider session
-- ids with no way to resume.
--
-- A row is inserted inside the SAME transaction as the state change, so the
-- claim "cleanup is owed for these runs" becomes durable exactly when the state
-- change does. The row is deleted once every step has succeeded; a partial or
-- failed run leaves it in place with `attempts`/`last_error` recorded, which is
-- what makes a failed `clear_sessions_for_dispatches_pg` retry-eligible instead
-- of a warning string. A restarted process drains the leftovers.
CREATE TABLE auto_queue_run_cleanup_tasks (
    id BIGSERIAL PRIMARY KEY,
    -- Runs whose slots may still be held. Slot release is CAS-guarded on
    -- `assigned_run_id = ANY(run_ids)` so a replay can never steal a slot that
    -- has since been handed to a different run.
    run_ids TEXT[] NOT NULL,
    -- Dispatches cancelled by the committed transaction; their `sessions` rows
    -- still need `claude_session_id`/`active_dispatch_id` cleared.
    dispatch_ids TEXT[] NOT NULL DEFAULT '{}',
    -- [{"agent_id": "...", "slot_index": 0}] — slots this task has already
    -- released. Persisted before slot-thread clearing so a crash between the
    -- release and the thread clear can still find the slots to clean.
    released_slots JSONB NOT NULL DEFAULT '[]'::JSONB,
    -- Serialized `CancelTransitionMeta` values whose observability emit is
    -- still owed.
    pending_emits JSONB NOT NULL DEFAULT '[]'::JSONB,
    -- Set once the emits have been fired so a replay does not repeat them.
    emitted BOOLEAN NOT NULL DEFAULT FALSE,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Drain order for the replay sweep.
CREATE INDEX auto_queue_run_cleanup_tasks_drain_idx
    ON auto_queue_run_cleanup_tasks (created_at ASC, id ASC);
