-- Routine agent fallback/retry durability.
--
-- The retry wait state stays on the running routine_runs row so the parent
-- routines.in_flight_run_id guard continues to prevent duplicate claims.

ALTER TABLE IF EXISTS routines
    ADD COLUMN IF NOT EXISTS fallback_agent_id TEXT REFERENCES agents(id) ON DELETE SET NULL;

ALTER TABLE IF EXISTS routines
    ADD COLUMN IF NOT EXISTS max_retries INTEGER NOT NULL DEFAULT 0;

ALTER TABLE IF EXISTS routine_runs
    ADD COLUMN IF NOT EXISTS retry_count INTEGER NOT NULL DEFAULT 0; -- agentdesk-audit: allow-int4 (retry_count is bounded by max_retries; small retry counter, not unbounded growth)

ALTER TABLE IF EXISTS routine_runs
    ADD COLUMN IF NOT EXISTS next_retry_at TIMESTAMPTZ;

ALTER TABLE IF EXISTS routine_runs
    ADD COLUMN IF NOT EXISTS attempts JSONB NOT NULL DEFAULT '[]'::jsonb;

CREATE INDEX IF NOT EXISTS idx_routine_runs_running_agent_retry
    ON routine_runs(next_retry_at)
    WHERE status = 'running'
      AND action = 'agent'
      AND turn_id IS NULL
      AND next_retry_at IS NOT NULL;
