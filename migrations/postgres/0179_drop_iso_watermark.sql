-- no-transaction
-- Reverse of the UNIQUE iso_unique_generation; the owner reads (provider, channel equal, ORDER BY generation DESC) scan that index backward.
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
DROP INDEX CONCURRENTLY IF EXISTS iso_watermark;
