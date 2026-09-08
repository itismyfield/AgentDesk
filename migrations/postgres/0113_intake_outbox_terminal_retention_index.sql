-- no-transaction
-- #5320 slice 1: time-ordered terminal keys for a later retention scan.
-- No delete behaviour is added. Both candidate queries in
-- sweep_failed_pre_accept_once can use this index: their status predicate
-- implies this partial predicate and their ORDER BY matches its keys.
-- PG 17.10 EXPLAIN (ANALYZE, BUFFERS), 500000 old done + 32 recent failures:
-- before/after, both custom/generic candidates kept failed_pre_accept_sweep
-- plus Sort, scanning 32 parent rows. This fixture does not guarantee plans
-- for other distributions or statistics. Raw evidence is in PR #5798 r2.
-- Terminal states are enumerated so new states do not silently enter the index.
--
-- Like 0108, run outside a transaction with one executable statement and no
-- conditional existence clause: a leftover INVALID index must hard-fail.
-- Inspect pg_index.indisvalid. If INVALID, DROP INDEX CONCURRENTLY
-- idx_intake_outbox_terminal_retention (or REINDEX INDEX CONCURRENTLY when
-- applicable), resolve the original failure, and rerun. A repaired valid index
-- still needs bookkeeping reconciliation before rerunning this CREATE.
-- If valid but unrecorded in _sqlx_migrations, either verify its definition and
-- record 0113 with the runner's matching checksum, or drop it concurrently and
-- rerun. Prefer drop/rerun when bookkeeping reconciliation is uncertain.
CREATE INDEX CONCURRENTLY idx_intake_outbox_terminal_retention
    ON intake_outbox (updated_at, id)
    WHERE status IN ('done', 'unknown', 'failed_pre_accept', 'failed_post_accept');
