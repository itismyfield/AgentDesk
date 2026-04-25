-- #1072 turn-lifecycle SLO aggregates + alert cooldown state (Epic #905 Phase 1).
CREATE TABLE IF NOT EXISTS slo_aggregates (
    id               BIGSERIAL PRIMARY KEY,
    window_start_ms  BIGINT NOT NULL,
    window_end_ms    BIGINT NOT NULL,
    metric           TEXT NOT NULL,
    value            DOUBLE PRECISION NOT NULL,
    sample_size      BIGINT NOT NULL DEFAULT 0,
    threshold        DOUBLE PRECISION,
    breached         BOOLEAN NOT NULL DEFAULT FALSE,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_slo_aggregates_metric_window
    ON slo_aggregates(metric, window_end_ms DESC);

CREATE INDEX IF NOT EXISTS idx_slo_aggregates_created_at
    ON slo_aggregates(created_at DESC);

-- Per-(metric, channel) cooldown tracker so repeat alerts within the
-- 30 minute window are suppressed.  `alerted_at_ms` holds the last time the
-- alert actually went out, not just when the breach was observed.
CREATE TABLE IF NOT EXISTS slo_alert_cooldowns (
    metric         TEXT NOT NULL,
    channel_id     TEXT NOT NULL,
    alerted_at_ms  BIGINT NOT NULL,
    last_value     DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (metric, channel_id)
);
