-- #5357: extend durable outbox with card rollback
--
-- `terminalize_selected_runs_with_pg` selects rollback candidates from the
-- current `dispatched|user_cancelled` entries and commits them to `skipped`
-- in the same transaction. Replay-safe card rollback runs post-commit, so a
-- crash leaves the entries changed with cards stuck in `requested|in_progress`.
-- The fix extends the cleanup task outbox with Some-generation rollbacks;
-- NULL-generation rollbacks instead run in the cancel transaction itself.
--
-- P1-B: outbox cards are identified by (card_id, dispatch_id) pairs with a
-- non-NULL dispatch_id generation. A successful rollback clears
-- latest_dispatch_id to NULL, so that token self-invalidates and a replay skips
-- on mismatch. NULL cannot self-invalidate (NULL -> rollback -> NULL), so it is
-- excluded at enrollment and rolled back synchronously in the cancel
-- transaction. A crash then commits neither the cancel nor the NULL rollback.

ALTER TABLE auto_queue_run_cleanup_tasks
ADD COLUMN card_rollback_tasks JSONB NOT NULL DEFAULT '[]',
ADD COLUMN card_rollback_source TEXT;

COMMENT ON COLUMN auto_queue_run_cleanup_tasks.card_rollback_tasks IS
    'Array of {card_id, dispatch_id} objects for cards that need status rollback from requested|in_progress to ready. dispatch_id is a non-NULL post-terminalization latest_dispatch_id generation snapshot; NULL generations are rolled back in the cancel transaction and must not be enrolled.';
COMMENT ON COLUMN auto_queue_run_cleanup_tasks.card_rollback_source IS
    'Source identifier for the card rollback (e.g., "auto_queue_cancel").';
