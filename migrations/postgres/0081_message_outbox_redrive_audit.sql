CREATE TABLE IF NOT EXISTS message_outbox_redrive_audit (
    id                BIGSERIAL PRIMARY KEY,
    message_outbox_id BIGINT NOT NULL,
    idempotency_key   TEXT NOT NULL,
    reason            TEXT NOT NULL,
    outcome           TEXT NOT NULL DEFAULT 'claimed',
    requested_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at      TIMESTAMPTZ,
    CONSTRAINT uq_message_outbox_redrive_idempotency
        UNIQUE (message_outbox_id, idempotency_key)
);

CREATE INDEX IF NOT EXISTS idx_message_outbox_redrive_audit_requested
    ON message_outbox_redrive_audit(requested_at DESC);
