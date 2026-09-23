-- #6047 — count redelivery claims so the sweep can order by attempts.
--
-- Claiming by id alone keeps a row that settles back to 'pending' at the head
-- of every batch; a batch of such rows shuts never-tried rows out until their
-- recency window closes. Ordering by (redelivery_attempts, id) claims every
-- never-tried row first and rotates the retried ones.
ALTER TABLE relay_dead_letter
    ADD COLUMN IF NOT EXISTS redelivery_attempts INTEGER NOT NULL DEFAULT 0;
