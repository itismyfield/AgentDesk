-- no-transaction
-- The weekly retention job deletes aged `agent_recovery_checkpoint_events` rows by `at`.
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
CREATE INDEX CONCURRENTLY idx_arce_at
    ON agent_recovery_checkpoint_events (at);
