-- #5357: extend durable outbox with card rollback
--
-- `terminalize_selected_runs_with_pg` selects rollback candidates from the
-- current `dispatched|user_cancelled` entries and commits them to `skipped`
-- in the same transaction. Card rollback runs post-commit, so a crash leaves
-- the entries changed with cards stuck in `requested|in_progress`. The fix
-- extends the cleanup task outbox to include card rollback, so it becomes
-- durable with the entry state change and is retried by the same drain machinery.

ALTER TABLE auto_queue_run_cleanup_tasks
ADD COLUMN card_ids TEXT[] NOT NULL DEFAULT '{}',
ADD COLUMN card_rollback_source TEXT;

COMMENT ON COLUMN auto_queue_run_cleanup_tasks.card_ids IS
    'Kanban cards that need status rollback from requested|in_progress to ready.';
COMMENT ON COLUMN auto_queue_run_cleanup_tasks.card_rollback_source IS
    'Source identifier for the card rollback (e.g., "auto_queue_cancel").';
