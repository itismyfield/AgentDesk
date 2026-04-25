-- #1101 agent_quality_daily extended metrics.
--
-- Adds the remaining DoD columns on top of #930's base rollup:
--   * avg_rework_count     — avg review_fail count per card that was reworked.
--   * cost_per_done_card   — sum(payload->>'cost') / count(card_transitioned→done).
--   * latency_p50_ms       — percentile_cont(0.5) over turn_complete.duration_ms.
--   * latency_p99_ms       — percentile_cont(0.99) over turn_complete.duration_ms.
--
-- All columns are nullable; the rollup writes NULL when the per-window sample
-- guard (QUALITY_SAMPLE_GUARD, default 5) is not met. #1101 populates these
-- incrementally as the supporting payload fields are landed by event emitters.

ALTER TABLE agent_quality_daily
    ADD COLUMN IF NOT EXISTS avg_rework_count    DOUBLE PRECISION,
    ADD COLUMN IF NOT EXISTS cost_per_done_card  DOUBLE PRECISION,
    ADD COLUMN IF NOT EXISTS latency_p50_ms      BIGINT,
    ADD COLUMN IF NOT EXISTS latency_p99_ms      BIGINT;
