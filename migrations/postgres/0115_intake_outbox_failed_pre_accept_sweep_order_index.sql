-- no-transaction
-- #5320: ordered sweep candidate limited to failed_pre_accept rows, so old
-- done/unknown/failed_post_accept rows need not be filtered from 0113's index.
-- Keep 0052's retry_count index for other query shapes. Planner choice remains
-- cost-based, and the number of failed_pre_accept rows is not capped here.
--
-- One executable statement, outside a transaction, with no existence bypass:
-- a leftover INVALID index must hard-fail. Inspect pg_index.indisvalid.
-- If INVALID, DROP INDEX CONCURRENTLY
-- idx_intake_outbox_failed_pre_accept_sweep_order, resolve the cause, and rerun.
-- REINDEX INDEX CONCURRENTLY may repair it, but then follow the valid branch:
-- if valid but unrecorded in _sqlx_migrations, verify its definition and record
-- 0115 with the runner's matching checksum, or drop concurrently and rerun.
-- Prefer drop/rerun when bookkeeping reconciliation is uncertain.
CREATE INDEX CONCURRENTLY idx_intake_outbox_failed_pre_accept_sweep_order
    ON intake_outbox (updated_at, id)
    WHERE status = 'failed_pre_accept';
