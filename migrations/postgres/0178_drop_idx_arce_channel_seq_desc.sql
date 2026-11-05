-- no-transaction
-- Reverse of the UNIQUE uq_arce_channel_seq; its only reader (channel_id = $1 ORDER BY seq DESC) scans that index backward.
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
DROP INDEX CONCURRENTLY IF EXISTS idx_arce_channel_seq_desc;
