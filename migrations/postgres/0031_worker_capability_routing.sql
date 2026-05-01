CREATE TABLE IF NOT EXISTS worker_mcp_endpoints (
    instance_id     TEXT NOT NULL,
    endpoint_name   TEXT NOT NULL,
    healthy         BOOLEAN,
    metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
    last_checked_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at      TIMESTAMPTZ DEFAULT NOW(),
    PRIMARY KEY (instance_id, endpoint_name),
    FOREIGN KEY (instance_id) REFERENCES worker_nodes(instance_id) ON DELETE CASCADE
);

ALTER TABLE task_dispatches
    ADD COLUMN IF NOT EXISTS required_capabilities JSONB,
    ADD COLUMN IF NOT EXISTS routing_diagnostics JSONB;

ALTER TABLE dispatch_outbox
    ADD COLUMN IF NOT EXISTS required_capabilities JSONB,
    ADD COLUMN IF NOT EXISTS routing_diagnostics JSONB;

CREATE INDEX IF NOT EXISTS idx_worker_mcp_endpoints_endpoint_healthy
    ON worker_mcp_endpoints(endpoint_name, healthy);

CREATE INDEX IF NOT EXISTS idx_dispatch_outbox_required_capabilities
    ON dispatch_outbox USING GIN (required_capabilities);
