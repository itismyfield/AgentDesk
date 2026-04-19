ALTER TABLE IF EXISTS message_outbox
    ADD COLUMN IF NOT EXISTS reason_code TEXT;

ALTER TABLE IF EXISTS message_outbox
    ADD COLUMN IF NOT EXISTS session_key TEXT;
