-- no-transaction
-- #5071 T2-W S-R1: support bounded stale-dispatched scans without rebuilding
-- the existing five-state open-route fence.
--
-- PostgreSQL requires CONCURRENTLY outside a transaction block. Keep this file
-- to this single executable statement. A failed concurrent build can leave an
-- INVALID index, so a conditional existence clause is intentionally omitted:
-- rerunning must hard-fail instead of letting SQLx record the migration over an
-- unusable index. Inspect pg_index.indisvalid. If INVALID, DROP INDEX
-- CONCURRENTLY idx_intake_outbox_stale_dispatched (or REINDEX INDEX
-- CONCURRENTLY when applicable), resolve the original failure, and rerun. If
-- valid but unrecorded, either keep it and mark 0108 applied or drop it
-- concurrently and rerun.
CREATE INDEX CONCURRENTLY idx_intake_outbox_stale_dispatched
    ON intake_outbox (dispatched_at) WHERE status='dispatched';
