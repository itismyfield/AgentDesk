-- no-transaction
-- #5071 T2-M stage 1: build the widened open-route fence before changing
-- the status CHECK. No production path writes `dispatched` in this slice, so
-- the existing four-status fence remains authoritative and the new predicate is
-- equivalent for all rows that the current CHECK admits.
--
-- PostgreSQL requires CONCURRENTLY to run outside a transaction block. Keep
-- this file to this single statement: SQLx 0.8.6 sends a migration file as one
-- Simple Query, and PostgreSQL treats multiple statements in one such message
-- as an implicit transaction block. The build performs its heap scans without a
-- lock that prevents ordinary SELECT, INSERT, UPDATE, or DELETE operations, but
-- it can wait for older transactions and add CPU/I/O load.
--
-- Failure recovery follows 0095_intake_outbox_idempotency_key_index.sql.
-- PostgreSQL can leave an INVALID index after a failed concurrent build, so
-- IF NOT EXISTS is intentionally omitted: a rerun must fail closed rather than
-- silently let 0106 swap in an incomplete fence. Inspect pg_index.indisvalid for
-- intake_outbox_one_open_route_per_channel_t2m. If it is INVALID, run
-- `DROP INDEX CONCURRENTLY intake_outbox_one_open_route_per_channel_t2m`
-- (or `REINDEX INDEX CONCURRENTLY` when applicable), resolve the original
-- failure, and rerun 0105. If it is valid because the build completed but SQLx
-- did not record 0105, either keep it and mark 0105 applied, or drop it
-- concurrently and rerun. Do not run 0106 until 0105 is recorded successfully.
CREATE UNIQUE INDEX CONCURRENTLY intake_outbox_one_open_route_per_channel_t2m
    ON intake_outbox (channel_id)
    WHERE status IN ('pending', 'claimed', 'accepted', 'spawned', 'dispatched');
