-- no-transaction
-- Leading prefix of the UNIQUE (entry_id, dispatch_id), which also serves the entry_id FK check.
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
DROP INDEX CONCURRENTLY IF EXISTS idx_aq_entry_dispatch_history_entry;
