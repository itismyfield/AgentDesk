-- The key row also serializes creates before a definition exists.
CREATE TABLE server_campaign_keys (
    campaign_id TEXT PRIMARY KEY
);

CREATE TABLE server_campaign_runs (
    campaign_id TEXT NOT NULL REFERENCES server_campaign_keys(campaign_id),
    run_id TEXT NOT NULL,
    document JSONB NOT NULL,
    nodes JSONB NOT NULL,
    definition_pin JSONB NOT NULL,
    PRIMARY KEY (campaign_id, run_id)
);

CREATE TABLE server_campaign_definitions (
    campaign_id TEXT PRIMARY KEY REFERENCES server_campaign_keys(campaign_id),
    revision BIGINT NOT NULL CHECK (revision > 0),
    definition JSONB NOT NULL,
    control JSONB NOT NULL,
    run_id TEXT,
    nodes JSONB NOT NULL,
    event_sequence BIGINT NOT NULL CHECK (event_sequence > 0),
    last_event_at TIMESTAMPTZ NOT NULL,
    FOREIGN KEY (campaign_id, run_id) REFERENCES server_campaign_runs(campaign_id, run_id)
);

-- Only these definition snapshots are pruned; execution records have no FK to them.
CREATE TABLE server_campaign_revisions (
    campaign_id TEXT NOT NULL REFERENCES server_campaign_keys(campaign_id),
    revision BIGINT NOT NULL CHECK (revision > 0),
    definition JSONB NOT NULL,
    change JSONB,
    PRIMARY KEY (campaign_id, revision)
);

CREATE TABLE server_campaign_run_history (
    campaign_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    document JSONB NOT NULL,
    nodes JSONB NOT NULL,
    definition_pin JSONB NOT NULL,
    PRIMARY KEY (campaign_id, run_id, sequence),
    FOREIGN KEY (campaign_id, run_id) REFERENCES server_campaign_runs(campaign_id, run_id)
);

CREATE TABLE server_campaign_attempts (
    campaign_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    document JSONB NOT NULL,
    definition_pin JSONB NOT NULL,
    PRIMARY KEY (campaign_id, run_id, attempt_id),
    FOREIGN KEY (campaign_id, run_id) REFERENCES server_campaign_runs(campaign_id, run_id)
);

CREATE TABLE server_campaign_actions (
    campaign_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    action_id TEXT NOT NULL,
    document JSONB NOT NULL,
    PRIMARY KEY (campaign_id, run_id, action_id),
    FOREIGN KEY (campaign_id, run_id) REFERENCES server_campaign_runs(campaign_id, run_id)
);

CREATE TABLE server_campaign_action_history (
    campaign_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    action_id TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    document JSONB NOT NULL,
    PRIMARY KEY (campaign_id, run_id, action_id, sequence),
    FOREIGN KEY (campaign_id, run_id, action_id)
        REFERENCES server_campaign_actions(campaign_id, run_id, action_id)
);

-- Opaque source documents are preserved here; F3 owns evidence acceptance.
CREATE TABLE server_campaign_evidence (
    campaign_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    evidence_id TEXT NOT NULL,
    document JSONB NOT NULL,
    PRIMARY KEY (campaign_id, run_id, evidence_id),
    FOREIGN KEY (campaign_id, run_id, attempt_id)
        REFERENCES server_campaign_attempts(campaign_id, run_id, attempt_id)
);

CREATE TABLE server_campaign_events (
    campaign_id TEXT NOT NULL REFERENCES server_campaign_keys(campaign_id),
    sequence BIGINT NOT NULL CHECK (sequence > 0),
    run_id TEXT,
    document JSONB NOT NULL,
    PRIMARY KEY (campaign_id, sequence),
    FOREIGN KEY (campaign_id, run_id) REFERENCES server_campaign_runs(campaign_id, run_id)
);
CREATE INDEX server_campaign_events_run ON server_campaign_events(campaign_id, run_id, sequence);

CREATE TABLE server_campaign_receipts (
    campaign_id TEXT NOT NULL REFERENCES server_campaign_keys(campaign_id),
    idempotency_key TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    outcome JSONB NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (campaign_id, idempotency_key)
);
