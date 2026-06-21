-- Routine pause-cause field: distinguishes failure-induced pauses from manual
-- operator pauses and migration-invalid pauses. Required for safe opt-in
-- auto-resume (#3573) — the prior `last_result` heuristic had both false
-- positives (manual pause after a past failure) and false negatives (migrated.rs
-- validation failures that left no marker).
--
-- Values:
--   'failure'           -- set by fail_run_and_pause_routine (run failed/timed-out)
--   'manual'            -- set by pause_routine (operator-initiated)
--   'migration_invalid' -- set when a migrated launchd run is blocked at validation
--   NULL                -- pre-existing paused rows (unknown cause; treated conservatively
--                          as manual — NOT eligible for auto-resume)

ALTER TABLE IF EXISTS routines
    ADD COLUMN IF NOT EXISTS pause_reason TEXT;
