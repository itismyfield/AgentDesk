-- no-transaction

-- #4538 PR-A — online idempotency dedup index for intake_outbox.
--
-- Migration 0094 adds idempotency_key as a nullable catalog-only column. A
-- partial index still has to scan every existing heap tuple to evaluate its
-- predicate, so building it inside 0094 would extend that transaction's
-- ACCESS EXCLUSIVE lock and a non-concurrent CREATE INDEX would block writes.
-- CONCURRENTLY performs the required scans without a write-blocking table lock.
--
-- Failure recovery: PostgreSQL can leave an INVALID index behind after a failed
-- concurrent build. IF NOT EXISTS is intentionally omitted so that a rerun or
-- supervisor auto-restart HARD-FAILS with 'relation
-- "intake_outbox_idempotency_key_uq" already exists' instead of silently
-- skipping the build and letting SQLx record 0095 as applied over a broken,
-- non-unique fence (fail-closed). The migration therefore can never be recorded
-- as applied until an operator resolves the leftover index explicitly. Inspect
-- pg_index.indisvalid: if this index is INVALID, DROP INDEX CONCURRENTLY
-- intake_outbox_idempotency_key_uq (or REINDEX INDEX CONCURRENTLY it when
-- applicable), then rerun this migration after resolving the original failure.
-- If the index is instead valid but unrecorded (the concurrent build finished
-- but SQLx never recorded 0095), either keep the index and mark this migration
-- applied, or DROP INDEX CONCURRENTLY it and rerun.
--
-- Ambiguous-commit retries reuse the same
-- (provider, channel, user_msg, attempt_no) key (§3.8); legacy NULL keys remain
-- outside the sparse unique index.
CREATE UNIQUE INDEX CONCURRENTLY intake_outbox_idempotency_key_uq
    ON intake_outbox (idempotency_key)
    WHERE idempotency_key IS NOT NULL;
