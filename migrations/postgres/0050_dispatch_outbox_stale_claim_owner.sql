CREATE INDEX IF NOT EXISTS idx_dispatch_outbox_pending_claim_owner
    ON dispatch_outbox (claim_owner, id)
    WHERE claim_owner IS NOT NULL
      AND status = 'pending';
