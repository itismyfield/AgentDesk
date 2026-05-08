ALTER TABLE dispatch_outbox
    ADD COLUMN IF NOT EXISTS wait_reason TEXT,
    ADD COLUMN IF NOT EXISTS wait_started_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS wake_up_history JSONB NOT NULL DEFAULT '[]'::jsonb;

CREATE INDEX IF NOT EXISTS idx_dispatch_outbox_wait_queue_fifo
    ON dispatch_outbox (created_at, id)
    WHERE status = 'pending'
      AND wait_reason IS NOT NULL;
