ALTER TABLE pr_tracking
    ADD COLUMN IF NOT EXISTS dispatch_generation TEXT NOT NULL DEFAULT '';

ALTER TABLE pr_tracking
    ADD COLUMN IF NOT EXISTS review_round INTEGER NOT NULL DEFAULT 0;

ALTER TABLE pr_tracking
    ADD COLUMN IF NOT EXISTS retry_count INTEGER NOT NULL DEFAULT 0;

CREATE UNIQUE INDEX IF NOT EXISTS idx_single_active_create_pr
    ON task_dispatches (kanban_card_id)
    WHERE dispatch_type = 'create-pr' AND status IN ('pending', 'dispatched');
