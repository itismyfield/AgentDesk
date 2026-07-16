-- Preserve the leader's author classification across multinode intake forwarding.
-- NULL identifies legacy rows (or rows written by an older producer); new workers
-- keep the historical fail-safe behavior by interpreting NULL as false.
ALTER TABLE intake_outbox
    ADD COLUMN IF NOT EXISTS preserve_on_cancel BOOLEAN;
