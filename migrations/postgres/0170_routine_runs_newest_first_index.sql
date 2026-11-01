-- Run history and each routine's newest run read routine_runs newest first; this index serves both
-- and replaces the routine_id-only index it starts with.
CREATE INDEX IF NOT EXISTS idx_routine_runs_routine_newest
    ON routine_runs(routine_id, started_at DESC, created_at DESC);
DROP INDEX IF EXISTS idx_routine_runs_routine_id;
