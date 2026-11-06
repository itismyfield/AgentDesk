-- no-transaction
-- The weekly retention job deletes aged `kanban_audit_logs` rows by `created_at`.
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
CREATE INDEX CONCURRENTLY idx_kanban_audit_logs_created_at
    ON kanban_audit_logs (created_at);
