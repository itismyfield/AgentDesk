-- Deleting a card now only stamps deleted_at, so dispatches and other rows that point
-- at it keep a valid reference instead of failing the delete or being orphaned.
ALTER TABLE kanban_cards ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;

-- Registering a deleted card's issue again still creates a fresh card.
DROP INDEX IF EXISTS idx_kanban_cards_repo_issue_unique;
CREATE UNIQUE INDEX IF NOT EXISTS idx_kanban_cards_repo_issue_unique
    ON kanban_cards (repo_id, github_issue_number)
    WHERE repo_id IS NOT NULL AND github_issue_number IS NOT NULL AND deleted_at IS NULL;
