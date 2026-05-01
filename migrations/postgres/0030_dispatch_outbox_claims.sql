ALTER TABLE dispatch_outbox
    ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS claim_owner TEXT;

CREATE INDEX IF NOT EXISTS idx_dispatch_outbox_status_claimed_at
    ON dispatch_outbox (status, claimed_at, next_attempt_at, id);
