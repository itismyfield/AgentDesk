ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS last_tool_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS active_children INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS parent_session_id BIGINT REFERENCES sessions(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS spawned_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS closed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS purpose TEXT;

CREATE INDEX IF NOT EXISTS idx_sessions_last_tool_at
    ON sessions(last_tool_at DESC);

CREATE INDEX IF NOT EXISTS idx_sessions_parent_session_id
    ON sessions(parent_session_id)
    WHERE parent_session_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_sessions_active_children
    ON sessions(active_children)
    WHERE active_children > 0;
