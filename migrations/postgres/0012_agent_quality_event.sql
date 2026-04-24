DO $$
BEGIN
    CREATE TYPE agent_quality_event_type AS ENUM (
        'turn_start',
        'turn_complete',
        'turn_error',
        'review_pass',
        'review_fail',
        'dispatch_dispatched',
        'dispatch_completed',
        'recovery_fired',
        'escalation',
        'card_transitioned',
        'stream_reattached',
        'watcher_lost',
        'outbox_delivery_failed',
        'ci_check_red',
        'queue_stuck'
    );
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

CREATE TABLE IF NOT EXISTS agent_quality_event (
    id              BIGSERIAL PRIMARY KEY,
    source_event_id TEXT,
    correlation_id  TEXT,
    agent_id        TEXT,
    provider        TEXT,
    channel_id      TEXT,
    card_id         TEXT,
    dispatch_id     TEXT,
    event_type      agent_quality_event_type NOT NULL,
    payload         JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_agent_quality_event_agent_created
    ON agent_quality_event(agent_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_agent_quality_event_created
    ON agent_quality_event(created_at DESC);

CREATE INDEX IF NOT EXISTS idx_agent_quality_event_dispatch
    ON agent_quality_event(dispatch_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_agent_quality_event_card
    ON agent_quality_event(card_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_agent_quality_event_correlation
    ON agent_quality_event(correlation_id, created_at DESC);
