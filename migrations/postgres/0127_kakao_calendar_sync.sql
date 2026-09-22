-- Managed calendars have durable intent/identity/dispatch evidence, independent of message outboxes.
CREATE TABLE kakao_calendar_bindings (
    account_id TEXT PRIMARY KEY CHECK (account_id ~ '^[a-z0-9][a-z0-9-]{0,31}$'),
    binding_id UUID NOT NULL UNIQUE,
    app_id BIGINT NOT NULL,
    user_id BIGINT NOT NULL,
    checked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (app_id, user_id)
);
CREATE TABLE kakao_calendar_events (
    id UUID PRIMARY KEY,
    revision BIGINT NOT NULL CHECK (revision > 0),
    content JSONB NOT NULL,
    deleted BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE kakao_calendar_targets (
    id UUID PRIMARY KEY,
    event_id UUID NOT NULL REFERENCES kakao_calendar_events(id),
    binding_id UUID NOT NULL REFERENCES kakao_calendar_bindings(binding_id),
    remote_id TEXT,
    applied_revision BIGINT NOT NULL DEFAULT 0,
    UNIQUE (event_id, binding_id),
    UNIQUE (binding_id, remote_id)
);
CREATE TABLE kakao_calendar_requests (
    request_key TEXT PRIMARY KEY CHECK (length(request_key) BETWEEN 1 AND 128),
    fingerprint TEXT NOT NULL,
    event_id UUID NOT NULL REFERENCES kakao_calendar_events(id) DEFERRABLE INITIALLY DEFERRED,
    revision BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE kakao_calendar_operations (
    id UUID PRIMARY KEY,
    target_id UUID NOT NULL REFERENCES kakao_calendar_targets(id),
    request_key TEXT NOT NULL REFERENCES kakao_calendar_requests(request_key),
    revision BIGINT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('create','update','delete')),
    snapshot JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','preparing','dispatching','applied','blocked','rejected','needs_reconcile','superseded')),
    claim_token UUID,
    lease_expires_at TIMESTAMPTZ,
    dispatched_at TIMESTAMPTZ,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    error_code TEXT,
    recovery_note TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (target_id, revision)
);
CREATE INDEX kakao_calendar_operations_queue ON kakao_calendar_operations(next_attempt_at, created_at) WHERE status = 'queued';
CREATE INDEX kakao_calendar_operations_target ON kakao_calendar_operations(target_id, revision);
