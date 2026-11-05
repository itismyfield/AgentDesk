-- no-transaction
-- Same columns and order as the UNIQUE constraint kakao_calendar_operations_target_id_revision_key.
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
DROP INDEX CONCURRENTLY IF EXISTS kakao_calendar_operations_target;
