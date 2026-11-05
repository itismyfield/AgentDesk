-- no-transaction
-- Card-scoped entry reads, the FK check from kanban_cards and the dispatch INSERT trigger's
-- entry lookup only had (run_id, kanban_card_id), which does not lead with the card.
--
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
-- IF NOT EXISTS is omitted on purpose: a failed concurrent build can leave an INVALID index,
-- and a rerun must hard-fail rather than let SQLx record 0175 over it. Inspect
-- pg_index.indisvalid. If INVALID, DROP INDEX CONCURRENTLY idx_auto_queue_entries_card
-- (or REINDEX INDEX CONCURRENTLY it), resolve the original failure, and rerun. If valid but
-- unrecorded, keep it and mark 0175 applied, or drop it concurrently and rerun.
CREATE INDEX CONCURRENTLY idx_auto_queue_entries_card
    ON auto_queue_entries (kanban_card_id);
