-- Track when a stuck-dispatch Discord alert has already been emitted, so the
-- watchdog can fire exactly once per stuck incident instead of every 5-minute
-- scan tick. The watchdog updates this column when it enqueues the alert and
-- the next scan filters those dispatches out until they leave the dispatched
-- state (which clears the column via ON CONFLICT-style updates upstream).

ALTER TABLE task_dispatches
    ADD COLUMN IF NOT EXISTS last_stuck_alert_at TIMESTAMPTZ;
