ALTER TABLE IF EXISTS message_outbox
    ADD COLUMN IF NOT EXISTS retry_count BIGINT NOT NULL DEFAULT 0;

ALTER TABLE IF EXISTS message_outbox
    ADD COLUMN IF NOT EXISTS next_attempt_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_message_outbox_pending_retry
    ON message_outbox(status, next_attempt_at, id);
