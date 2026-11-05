-- no-transaction
-- The weekly retention job deletes aged `review_tuning_outcomes` rows by `created_at`.
-- CONCURRENTLY must run outside a transaction, so this file holds one executable statement.
CREATE INDEX CONCURRENTLY idx_review_tuning_outcomes_created_at
    ON review_tuning_outcomes (created_at);
