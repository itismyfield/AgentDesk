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
    -- Availability controls, mirroring the `intake_outbox` convention.
    --
    -- Without them a permanently failing row keeps its place at the head of the
    -- drain order and starves every task queued behind it: the sweep reads the
    -- oldest rows first, so `REPLAY_BATCH_LIMIT` dead rows are enough to stop
    -- new cleanup from ever draining.
    --
    -- `next_attempt_at` is pushed into the future by each failure (exponential
    -- backoff), so a failing row yields its slot back to newer work.
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Row claim. The inline post-commit drain and the policy-tick replay sweep
    -- would otherwise both pick up the same row and emit its observability rows
    -- twice; the claim makes exactly one of them the owner. `claimed_at` doubles
    -- as the lease clock so a process that died mid-drain does not strand the row.
    claim_owner TEXT,
    claimed_at TIMESTAMPTZ,
    -- Dead letter. Set once `attempts` crosses the cap (~13-17 minutes of
    -- failing retries; see `MAX_BACKOFF_SECONDS`) or the payload cannot be
    -- decoded at all. The row is retained rather than deleted so the evidence
    -- survives, but it is excluded from the drain query, so nothing will ever
    -- retry it and the run's slot token and residual provider session id stay
    -- as the last failed attempt left them.
    --
    -- Retention only helps if someone can find the row, so this column is read
    -- by `dead_lettered_run_cleanup_task_count_pg` and surfaced on `/api/health`
    -- as `auto_queue_cleanup.dead_lettered`. A non-zero value there is an
    -- operator action item. Do not let this become a write-only column again.
    dead_lettered_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Drain order for the replay sweep. Partial on the live set so dead-lettered
-- rows leave the index entirely instead of being scanned and filtered forever.
CREATE INDEX auto_queue_run_cleanup_tasks_drain_idx
    ON auto_queue_run_cleanup_tasks (next_attempt_at ASC, created_at ASC, id ASC)
    WHERE dead_lettered_at IS NULL;
