-- Routine execution modes: additive definition fields and the per-run claim snapshot.
--
-- routines.execution_mode   NULL keeps the pre-mode (legacy) contract; existing rows are not backfilled.
-- routines.runner           discriminated runner JSON; only the script mode may hold a command runner.
-- routines.headless_failure_notice_limit  headless failure notices before escalation (1..100, default 3).
-- routine_runs.run_spec     definition pinned at claim; poll/recovery read it instead of the live row.

ALTER TABLE IF EXISTS routines
    ADD COLUMN IF NOT EXISTS execution_mode TEXT,
    ADD COLUMN IF NOT EXISTS runner JSONB,
    ADD COLUMN IF NOT EXISTS headless_failure_notice_limit INTEGER NOT NULL DEFAULT 3;

ALTER TABLE IF EXISTS routines
    ADD CONSTRAINT routines_execution_mode_check
        CHECK (execution_mode IS NULL OR execution_mode IN ('headless', 'thread', 'channel', 'script')),
    ADD CONSTRAINT routines_runner_shape_check
        CHECK (
            runner IS NULL
            OR (
                jsonb_typeof(runner) = 'object'
                AND runner ->> 'kind' IN ('quickjs', 'command')
                AND (runner ->> 'kind' <> 'command' OR execution_mode = 'script')
            )
        ),
    ADD CONSTRAINT routines_explicit_mode_fresh_check
        CHECK (execution_mode IS NULL OR execution_strategy = 'fresh'),
    ADD CONSTRAINT routines_headless_failure_notice_limit_check
        CHECK (headless_failure_notice_limit BETWEEN 1 AND 100);

ALTER TABLE IF EXISTS routine_runs
    ADD COLUMN IF NOT EXISTS run_spec JSONB;
