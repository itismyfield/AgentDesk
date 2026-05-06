ALTER TABLE IF EXISTS sessions
    ADD COLUMN IF NOT EXISTS active_turn_delivery_outbox_id BIGINT;

CREATE INDEX IF NOT EXISTS idx_sessions_active_turn_delivery_outbox_id
    ON sessions(active_turn_delivery_outbox_id)
    WHERE active_turn_delivery_outbox_id IS NOT NULL;
