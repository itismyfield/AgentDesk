-- #5941 Step B — redelivery bookkeeping for relay_dead_letter.
--
-- 0079 is append-only with no delivery state, so a reader cannot tell a row it
-- already replayed from one it has not. Recording is at-least-once and NOT
-- idempotent (invariant I17), so replaying rows verbatim duplicates the body in
-- the user's channel. These columns are the durable claim that makes redelivery
-- exactly-once: 'pending' -> 'claimed' under FOR UPDATE SKIP LOCKED, never back.
ALTER TABLE relay_dead_letter
    ADD COLUMN IF NOT EXISTS redelivery_state TEXT NOT NULL DEFAULT 'pending';

ALTER TABLE relay_dead_letter
    ADD COLUMN IF NOT EXISTS redelivered_at TIMESTAMPTZ;

-- The sweep reads pending rows of ONE kind over a recency window; the partial
-- predicate keeps the index at the size of the unclaimed backlog.
CREATE INDEX IF NOT EXISTS idx_relay_dead_letter_pending_redelivery
    ON relay_dead_letter (kind, created_at)
    WHERE redelivery_state = 'pending';
