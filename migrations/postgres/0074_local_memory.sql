-- Local fallback memory storage for /api/memory when Memento is unavailable
-- or ADK_FORCE_LOCAL_MEMORY=1 is set. Keep DDL in migrations so request
-- handlers never perform schema creation on the hot path.
CREATE TABLE IF NOT EXISTS local_memory (
    id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    topic TEXT NOT NULL,
    kind TEXT NOT NULL,
    importance DOUBLE PRECISION,
    workspace TEXT,
    keywords JSONB,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_local_memory_workspace_created_at
    ON local_memory (workspace, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_local_memory_created_at
    ON local_memory (created_at DESC);

CREATE INDEX IF NOT EXISTS idx_local_memory_topic
    ON local_memory (topic);

CREATE INDEX IF NOT EXISTS idx_local_memory_kind
    ON local_memory (kind);

CREATE INDEX IF NOT EXISTS idx_local_memory_keywords_gin
    ON local_memory USING GIN (keywords);
