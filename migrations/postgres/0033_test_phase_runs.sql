CREATE TABLE IF NOT EXISTS test_phase_runs (
    id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    phase_key TEXT NOT NULL,
    head_sha TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'running', 'passed', 'failed', 'canceled')),
    issue_id TEXT,
    card_id TEXT,
    repo_id TEXT,
    required_capabilities JSONB NOT NULL DEFAULT '{}'::jsonb,
    resource_lock_key TEXT,
    holder_instance_id TEXT,
    holder_job_id TEXT,
    evidence JSONB NOT NULL DEFAULT '{}'::jsonb,
    error TEXT,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_test_phase_runs_phase_head
    ON test_phase_runs(phase_key, head_sha);

CREATE INDEX IF NOT EXISTS idx_test_phase_runs_status_updated
    ON test_phase_runs(status, updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_test_phase_runs_required_capabilities
    ON test_phase_runs USING GIN (required_capabilities);
