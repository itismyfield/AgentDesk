-- One row per approved Decision input (a request's selection, or one opinion message), with the
-- requester's agent and logical channel copied here until decision_requests exists to own them.
CREATE TABLE IF NOT EXISTS decision_input_events (
    event_id UUID PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('selection', 'opinion')),
    source_key TEXT NOT NULL UNIQUE,
    origin TEXT NOT NULL UNIQUE,
    requester_agent_id TEXT NOT NULL,
    logical_channel_id TEXT NOT NULL,
    actor_user_id TEXT NOT NULL,
    text_sha256 TEXT NOT NULL,
    admission_sequence BIGINT GENERATED ALWAYS AS IDENTITY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- The single delivery workflow of an event. Only the not_started -> resolving update starts it;
-- there is no retry time, and a closed row never becomes runnable again.
CREATE TABLE IF NOT EXISTS decision_delivery_attempts (
    event_id UUID PRIMARY KEY REFERENCES decision_input_events (event_id),
    workflow_ticket TEXT,
    state TEXT NOT NULL DEFAULT 'not_started' CHECK (state IN (
        'not_started', 'resolving', 'normal_wait', 'normalizing', 'may_have_sent',
        'closing', 'delivered', 'failed', 'unknown_closed')),
    normalization_used BOOLEAN NOT NULL DEFAULT FALSE,
    attempt_turn_id TEXT,
    detail TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
