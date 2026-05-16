-- #2257 concern 5 rollback — drop the idempotency keys table.
--
-- Down migration is destructive: any in-flight idempotency contracts
-- callers were relying on are lost. The application code degrades
-- gracefully (handlers fall back to non-idempotent execution when the
-- table is absent), so the rollback is safe but you lose dedup until
-- you forward-migrate again.

DROP INDEX IF EXISTS idempotency_keys_expires_at_idx;
DROP TABLE IF EXISTS idempotency_keys;
