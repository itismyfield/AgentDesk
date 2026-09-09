-- no-transaction
-- #5320 slice 1: time-ordered terminal keys for a later retention scan.
-- No delete behaviour or retention reader is added. Migration 0115 provides
-- sweep_failed_pre_accept_once with the same order over failed_pre_accept only,
-- bounding that index scan to failed_pre_accept rows, not all terminal rows.
-- This broader index is intended for the later retention reader, but remains
-- an eligible planner candidate for sweep queries (index choice is cost-based).
-- Terminal states are enumerated so new states do not silently enter the index.
--
-- Like 0108, run outside a transaction with one executable statement and no
-- conditional existence clause: a leftover INVALID index must hard-fail.
-- Inspect pg_index.indisvalid. If INVALID, DROP INDEX CONCURRENTLY
-- idx_intake_outbox_terminal_retention (or REINDEX INDEX CONCURRENTLY when
-- applicable). After DROP, resolve the original failure and rerun. After
-- REINDEX, follow the valid-but-unrecorded procedure below instead of CREATE.
-- If valid but unrecorded in _sqlx_migrations, either verify its definition and
-- record 0113 with the runner's matching checksum, or drop it concurrently and
-- rerun. Prefer drop/rerun when bookkeeping reconciliation is uncertain.
CREATE INDEX CONCURRENTLY idx_intake_outbox_terminal_retention
    ON intake_outbox (updated_at, id)
    WHERE status IN ('done', 'unknown', 'failed_pre_accept', 'failed_post_accept');
