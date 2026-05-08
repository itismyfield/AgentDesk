CREATE TABLE IF NOT EXISTS dispatch_semaphore_holdings (
    semaphore_name     TEXT NOT NULL,
    scope              TEXT NOT NULL CHECK (scope IN ('per-node', 'per-cluster')),
    scope_key          TEXT NOT NULL,
    slot_index         INTEGER NOT NULL CHECK (slot_index >= 0),
    holder_instance_id TEXT NOT NULL,
    dispatch_id        TEXT NOT NULL REFERENCES task_dispatches(id) ON DELETE CASCADE,
    acquired_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at         TIMESTAMPTZ NOT NULL,
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (semaphore_name, scope, scope_key, slot_index)
);

CREATE INDEX IF NOT EXISTS idx_dispatch_semaphore_holdings_dispatch
    ON dispatch_semaphore_holdings(dispatch_id);

CREATE INDEX IF NOT EXISTS idx_dispatch_semaphore_holdings_expiry
    ON dispatch_semaphore_holdings(expires_at);

CREATE INDEX IF NOT EXISTS idx_dispatch_semaphore_holdings_holder
    ON dispatch_semaphore_holdings(holder_instance_id, expires_at);
