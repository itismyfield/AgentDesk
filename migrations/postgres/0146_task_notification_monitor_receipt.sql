-- Observations share the existing card authority; receipts survive card GC.
CREATE TABLE task_notification_monitor_receipt (
    id BIGSERIAL PRIMARY KEY,
    channel_id BIGINT NOT NULL CHECK (channel_id > 0),
    provider TEXT NOT NULL,
    session_key TEXT NOT NULL,
    event_key TEXT NOT NULL,
    payload_fingerprint VARCHAR(64) NOT NULL,
    source_fingerprint VARCHAR(64),
    legacy_render_count BIGINT CHECK (legacy_render_count IS NULL OR legacy_render_count >= 1),
    discord_message_id BIGINT NOT NULL CHECK (discord_message_id > 0),
    bot_key TEXT NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX task_notification_monitor_receipt_source
    ON task_notification_monitor_receipt (channel_id, provider, source_fingerprint)
    WHERE source_fingerprint IS NOT NULL;
CREATE UNIQUE INDEX task_notification_monitor_receipt_payload
    ON task_notification_monitor_receipt (channel_id, provider, event_key, payload_fingerprint)
    WHERE source_fingerprint IS NULL;
CREATE INDEX task_notification_monitor_receipt_lookup
    ON task_notification_monitor_receipt (channel_id, provider, event_key, payload_fingerprint);
