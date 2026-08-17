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
-- is the card's post-terminalization latest_dispatch_id snapshot (including
-- NULL) and acts as a generation marker. A successful rollback carrying Some
-- clears latest_dispatch_id to NULL, so that token self-invalidates and a replay
-- skips on mismatch. NULL cannot self-invalidate (NULL -> rollback -> NULL), so
-- it is applied only by the initial inline drain; replay or lease-expiry reclaim
-- dead-letters it without reapplying. Equality proves only that the current
-- value matches the snapshot, not which lifecycle produced that value.

ALTER TABLE auto_queue_run_cleanup_tasks
ADD COLUMN card_rollback_tasks JSONB NOT NULL DEFAULT '[]',
ADD COLUMN card_rollback_source TEXT;

COMMENT ON COLUMN auto_queue_run_cleanup_tasks.card_rollback_tasks IS
    'Array of {card_id, dispatch_id} objects for cards that need status rollback from requested|in_progress to ready. dispatch_id is the post-terminalization latest_dispatch_id generation snapshot and may be null.';
COMMENT ON COLUMN auto_queue_run_cleanup_tasks.card_rollback_source IS
    'Source identifier for the card rollback (e.g., "auto_queue_cancel").';
