-- #1104 (911-4) agent quality regression alert 24h cooldown.
--
-- Tracks the last time a regression alert fired for a given
-- (agent_id, metric) pair so the rule engine in
-- `src/services/agent_quality/regression_alerts.rs` can suppress
-- duplicate Discord notifications inside a 24 hour window.
--
-- Independent from the existing kv_meta-based dedupe used by
-- `enqueue_quality_regression_alerts_pg` (#1101): that path will be
-- routed through this table once #1101 callers migrate. Until then
-- the two cooldowns coexist; both default to 24h and key off
-- (agent_id, metric).

CREATE TABLE IF NOT EXISTS quality_regression_cooldowns (
    agent_id        TEXT NOT NULL,
    metric          TEXT NOT NULL,
    alerted_at_ms   BIGINT NOT NULL,
    last_baseline   DOUBLE PRECISION NOT NULL,
    last_current    DOUBLE PRECISION NOT NULL,
    last_delta      DOUBLE PRECISION NOT NULL,
    last_sample_size BIGINT NOT NULL,
    PRIMARY KEY (agent_id, metric)
);

CREATE INDEX IF NOT EXISTS idx_quality_regression_cooldowns_alerted
    ON quality_regression_cooldowns(alerted_at_ms DESC);
