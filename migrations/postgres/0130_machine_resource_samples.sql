-- One row per distinct sample published by a successful node heartbeat.
-- The stable node ID is retained as history even if a registry row is removed.
CREATE TABLE IF NOT EXISTS machine_resource_samples (
    instance_id TEXT NOT NULL,
    observed_at_ms BIGINT NOT NULL CHECK (observed_at_ms > 0),
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms >= observed_at_ms),
    resources JSONB NOT NULL CHECK (jsonb_typeof(resources) = 'object'),
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (instance_id, observed_at_ms)
);

CREATE INDEX IF NOT EXISTS idx_machine_resource_samples_retention
    ON machine_resource_samples (observed_at_ms);
