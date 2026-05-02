CREATE TABLE IF NOT EXISTS recovery_audit_records (
    id                    BIGSERIAL PRIMARY KEY,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    channel_id            TEXT NOT NULL,
    session_key           TEXT,
    source                TEXT NOT NULL,
    message_count         INTEGER NOT NULL,
    max_chars_per_message INTEGER NOT NULL,
    authors_json          JSONB NOT NULL DEFAULT '[]'::jsonb,
    redacted_preview      TEXT NOT NULL,
    content_sha256        TEXT NOT NULL,
    consumed_by_turn_id   TEXT
);

CREATE INDEX IF NOT EXISTS idx_recovery_audit_records_channel_id
    ON recovery_audit_records(channel_id);

CREATE INDEX IF NOT EXISTS idx_recovery_audit_records_created_at
    ON recovery_audit_records(created_at DESC);

CREATE INDEX IF NOT EXISTS idx_recovery_audit_records_consumed_by_turn_id
    ON recovery_audit_records(consumed_by_turn_id);
