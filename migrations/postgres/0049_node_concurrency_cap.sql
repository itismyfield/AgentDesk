CREATE INDEX IF NOT EXISTS idx_dispatch_outbox_claim_owner_active_status
    ON dispatch_outbox (claim_owner, status)
    WHERE claim_owner IS NOT NULL
      AND status IN ('claimed', 'processing');
