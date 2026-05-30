ALTER TABLE IF EXISTS message_outbox
    ADD COLUMN IF NOT EXISTS dedupe_key TEXT;

ALTER TABLE IF EXISTS message_outbox
    ADD COLUMN IF NOT EXISTS dedupe_expires_at TIMESTAMPTZ;

CREATE UNIQUE INDEX IF NOT EXISTS uq_message_outbox_active_dedupe_key
    ON message_outbox(dedupe_key)
    WHERE dedupe_key IS NOT NULL
      AND status != 'failed';

CREATE INDEX IF NOT EXISTS idx_message_outbox_dedupe_expiry
    ON message_outbox(dedupe_expires_at)
    WHERE dedupe_key IS NOT NULL;
