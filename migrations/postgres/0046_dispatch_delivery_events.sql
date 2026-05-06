CREATE TABLE IF NOT EXISTS dispatch_delivery_events (
    id                  BIGSERIAL PRIMARY KEY,
    dispatch_id         TEXT NOT NULL REFERENCES task_dispatches(id) ON DELETE CASCADE,
    correlation_id      TEXT NOT NULL,
    semantic_event_id   TEXT NOT NULL,
    operation           TEXT NOT NULL DEFAULT 'send',
    target_kind         TEXT NOT NULL DEFAULT 'channel',
    target_channel_id   TEXT,
    target_thread_id    TEXT,
    status              TEXT NOT NULL CHECK (
        status IN ('reserved', 'sent', 'fallback', 'duplicate', 'skipped', 'failed')
    ),
    attempt             INTEGER NOT NULL DEFAULT 1 CHECK (attempt > 0),
    message_id          TEXT,
    messages_json       JSONB NOT NULL DEFAULT '[]'::jsonb,
    fallback_kind       TEXT,
    error               TEXT,
    result_json         JSONB NOT NULL DEFAULT '{}'::jsonb,
    reserved_until      TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS uq_dispatch_delivery_events_attempt
    ON dispatch_delivery_events (
        correlation_id,
        semantic_event_id,
        operation,
        target_kind,
        (COALESCE(target_channel_id, '')),
        (COALESCE(target_thread_id, '')),
        attempt
    );

CREATE UNIQUE INDEX IF NOT EXISTS uq_dispatch_delivery_events_active
    ON dispatch_delivery_events (
        correlation_id,
        semantic_event_id,
        operation,
        target_kind,
        (COALESCE(target_channel_id, '')),
        (COALESCE(target_thread_id, ''))
    )
    WHERE status IN ('reserved', 'sent', 'fallback', 'duplicate', 'skipped');

CREATE INDEX IF NOT EXISTS idx_dispatch_delivery_events_dispatch_created
    ON dispatch_delivery_events (dispatch_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_dispatch_delivery_events_status_created
    ON dispatch_delivery_events (status, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_dispatch_delivery_events_reserved_until
    ON dispatch_delivery_events (reserved_until)
    WHERE status = 'reserved' AND reserved_until IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_dispatch_delivery_events_message_id
    ON dispatch_delivery_events (message_id)
    WHERE message_id IS NOT NULL;
