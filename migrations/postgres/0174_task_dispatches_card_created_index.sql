-- no-transaction
-- Card-scoped dispatch reads (WHERE kanban_card_id = $1, newest first) and the FK check from
-- kanban_cards had only partial indexes; btree scans this one backward for DESC.
--
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
-- IF NOT EXISTS is omitted on purpose: a failed concurrent build can leave an INVALID index,
-- and a rerun must hard-fail rather than let SQLx record 0174 over it. Inspect
-- pg_index.indisvalid. If INVALID, DROP INDEX CONCURRENTLY idx_task_dispatches_card_created
-- (or REINDEX INDEX CONCURRENTLY it), resolve the original failure, and rerun. If valid but
-- unrecorded, keep it and mark 0174 applied, or drop it concurrently and rerun.
CREATE INDEX CONCURRENTLY idx_task_dispatches_card_created
    ON task_dispatches (kanban_card_id, created_at);
