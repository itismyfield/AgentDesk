ALTER TABLE task_dispatches
    ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS claim_owner TEXT,
    ADD COLUMN IF NOT EXISTS claim_expires_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS idempotency_key TEXT,
    ADD COLUMN IF NOT EXISTS routing_diagnostics JSONB;

CREATE INDEX IF NOT EXISTS idx_task_dispatches_status_claim_expires
    ON task_dispatches(status, claim_expires_at, created_at);

CREATE UNIQUE INDEX IF NOT EXISTS uq_task_dispatches_idempotency_key
    ON task_dispatches(idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS test_results (
    id TEXT PRIMARY KEY,
    phase_run_id TEXT NOT NULL REFERENCES test_phase_runs(id) ON DELETE CASCADE,
    result_key TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('passed', 'failed', 'skipped')),
    summary TEXT,
    artifacts JSONB NOT NULL DEFAULT '{}'::jsonb,
    metrics JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (phase_run_id, result_key)
);

CREATE TABLE IF NOT EXISTS issue_specs (
    issue_id TEXT PRIMARY KEY,
    card_id TEXT,
    repo_id TEXT,
    issue_number INTEGER,
    head_sha TEXT,
    acceptance_criteria JSONB NOT NULL DEFAULT '[]'::jsonb,
    test_plan JSONB NOT NULL DEFAULT '[]'::jsonb,
    definition_of_done JSONB NOT NULL DEFAULT '[]'::jsonb,
    required_phases JSONB NOT NULL DEFAULT '[]'::jsonb,
    validation_errors JSONB NOT NULL DEFAULT '[]'::jsonb,
    source_body_sha TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_issue_specs_card_id
    ON issue_specs(card_id);

CREATE INDEX IF NOT EXISTS idx_issue_specs_required_phases
    ON issue_specs USING GIN (required_phases);
