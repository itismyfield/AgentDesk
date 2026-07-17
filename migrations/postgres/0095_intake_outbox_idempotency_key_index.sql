-- no-transaction

-- #4538 PR-A — online idempotency dedup index for intake_outbox.
--
-- Migration 0094 adds idempotency_key as a nullable catalog-only column. A
-- partial index still has to scan every existing heap tuple to evaluate its
-- predicate, so building it inside 0094 would extend that transaction's
-- ACCESS EXCLUSIVE lock and a non-concurrent CREATE INDEX would block writes.
-- CONCURRENTLY performs the required scans without a write-blocking table lock.
--
-- Failure recovery: PostgreSQL can leave an INVALID index after a failed
-- concurrent build. IF NOT EXISTS does not repair that index on retry. Inspect
-- pg_index.indisvalid; if this index is invalid, DROP INDEX CONCURRENTLY
-- intake_outbox_idempotency_key_uq (or REINDEX INDEX CONCURRENTLY it when
-- applicable), then rerun this migration after resolving the original failure.
--
-- Ambiguous-commit retries reuse the same
-- (provider, channel, user_msg, attempt_no) key (§3.8); legacy NULL keys remain
-- outside the sparse unique index.
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS intake_outbox_idempotency_key_uq
    ON intake_outbox (idempotency_key)
    WHERE idempotency_key IS NOT NULL;
