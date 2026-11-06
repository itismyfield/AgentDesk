-- no-transaction
-- Leading prefix of the UNIQUE (card_id, iteration), which also serves the card_id FK check.
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
DROP INDEX CONCURRENTLY IF EXISTS idx_aci_card_id;
