-- One row per Decision an agent asked its operator. The question is immutable, one idempotency
-- key names one request per requester scope, and the requester capability is kept as its hash.
CREATE TABLE IF NOT EXISTS decision_requests (
    request_id UUID PRIMARY KEY,
    requester_agent_id TEXT NOT NULL,
    logical_channel_id TEXT NOT NULL,
    target_channel_id TEXT NOT NULL,
    request_ref TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    payload_sha256 TEXT NOT NULL,
    capability_sha256 TEXT NOT NULL,
    title TEXT NOT NULL,
    context TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (requester_agent_id, logical_channel_id, idempotency_key)
);

-- The options of a request in the order they were asked; the percents are the helper's.
CREATE TABLE IF NOT EXISTS decision_options (
    request_id UUID NOT NULL REFERENCES decision_requests (request_id),
    ordinal SMALLINT NOT NULL CHECK (ordinal BETWEEN 1 AND 5),
    label TEXT NOT NULL,
    option_key TEXT,
    detail TEXT NOT NULL,
    recommendation_percent SMALLINT CHECK (recommendation_percent BETWEEN 0 AND 100),
    PRIMARY KEY (request_id, ordinal)
);
