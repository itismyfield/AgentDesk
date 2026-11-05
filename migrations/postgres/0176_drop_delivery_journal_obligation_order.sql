-- no-transaction
-- Same columns and order as the UNIQUE constraint delivery_journal_obligation_slot_unique.
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
DROP INDEX CONCURRENTLY IF EXISTS delivery_journal_obligation_order;
