-- #5357: extend durable outbox with card rollback
--
-- `terminalize_selected_runs_with_pg` selects rollback candidates from the
-- current `dispatched|user_cancelled` entries and commits them to `skipped`
-- in the same transaction. Card rollback runs post-commit, so a crash leaves
-- the entries changed with cards stuck in `requested|in_progress`. The fix
-- extends the cleanup task outbox to include card rollback, so it becomes
-- durable with the entry state change and is retried by the same drain machinery.
--
-- P1-B: cards are identified by (card_id, dispatch_id) pairs. The dispatch_id
-- acts as a generation marker: during drain, if the card's current
-- latest_dispatch_id differs from the stored dispatch_id, the card has been
-- reassigned to a new lifecycle and the rollback is skipped.

ALTER TABLE auto_queue_run_cleanup_tasks
ADD COLUMN card_rollback_tasks JSONB NOT NULL DEFAULT '[]',
ADD COLUMN card_rollback_source TEXT;

COMMENT ON COLUMN auto_queue_run_cleanup_tasks.card_rollback_tasks IS
    'Array of {card_id, dispatch_id} objects for cards that need status rollback from requested|in_progress to ready. dispatch_id is the generation marker.';
COMMENT ON COLUMN auto_queue_run_cleanup_tasks.card_rollback_source IS
    'Source identifier for the card rollback (e.g., "auto_queue_cancel").';
