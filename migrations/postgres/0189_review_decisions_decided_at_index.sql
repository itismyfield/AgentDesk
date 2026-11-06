-- no-transaction
-- The weekly retention job deletes aged `review_decisions` rows by `decided_at`.
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
CREATE INDEX CONCURRENTLY idx_review_decisions_decided_at
    ON review_decisions (decided_at);
