CREATE TABLE IF NOT EXISTS resource_locks (
    lock_key TEXT PRIMARY KEY,
    holder_instance_id TEXT NOT NULL,
    holder_job_id TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    expires_at TIMESTAMPTZ NOT NULL,
    heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_resource_locks_holder
    ON resource_locks(holder_instance_id, holder_job_id);

CREATE INDEX IF NOT EXISTS idx_resource_locks_expires_at
    ON resource_locks(expires_at);
