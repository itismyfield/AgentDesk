CREATE TABLE IF NOT EXISTS observability_events (
    id           BIGSERIAL PRIMARY KEY,
    event_type   TEXT NOT NULL,
    provider     TEXT,
    channel_id   TEXT,
    dispatch_id  TEXT,
    session_key  TEXT,
    turn_id      TEXT,
    status       TEXT,
    payload_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_observability_events_created_at
    ON observability_events(created_at DESC);

CREATE INDEX IF NOT EXISTS idx_observability_events_provider_channel
    ON observability_events(provider, channel_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_observability_events_dispatch_id
    ON observability_events(dispatch_id);

CREATE TABLE IF NOT EXISTS observability_counter_snapshots (
    id                   BIGSERIAL PRIMARY KEY,
    provider             TEXT NOT NULL,
    channel_id           TEXT NOT NULL,
    turn_attempts        BIGINT NOT NULL DEFAULT 0,
    guard_fires          BIGINT NOT NULL DEFAULT 0,
    watcher_replacements BIGINT NOT NULL DEFAULT 0,
    recovery_fires       BIGINT NOT NULL DEFAULT 0,
    turn_successes       BIGINT NOT NULL DEFAULT 0,
    turn_failures        BIGINT NOT NULL DEFAULT 0,
    snapshot_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_observability_counter_snapshots_provider_channel
    ON observability_counter_snapshots(provider, channel_id, snapshot_at DESC);

CREATE INDEX IF NOT EXISTS idx_observability_counter_snapshots_snapshot_at
    ON observability_counter_snapshots(snapshot_at DESC);
