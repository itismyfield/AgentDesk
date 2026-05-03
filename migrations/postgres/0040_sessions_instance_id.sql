ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS instance_id TEXT;

CREATE INDEX IF NOT EXISTS idx_sessions_instance_active
    ON sessions(instance_id, status)
    WHERE instance_id IS NOT NULL
      AND status IN ('connected', 'turn_active', 'awaiting_bg', 'awaiting_user', 'idle', 'working');
