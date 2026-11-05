-- no-transaction
-- Leading prefix of idx_turn_lifecycle_events_turn_channel_created_at.
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
DROP INDEX CONCURRENTLY IF EXISTS idx_turn_lifecycle_events_turn_id;
