-- Deleting a card now only stamps deleted_at, so dispatches and other rows that point
-- at it keep a valid reference instead of failing the delete or being orphaned.
ALTER TABLE kanban_cards ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;
