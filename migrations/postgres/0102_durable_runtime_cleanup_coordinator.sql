-- Task #25 Slice 1: dormant durable runtime cleanup coordinator foundation.
--
-- These tables are additive journal/authority primitives only. No production
-- consumer reads or writes them until a later rollout slice activates the saga.

CREATE TABLE runtime_cleanup_operations (
    operation_id UUID PRIMARY KEY,
    request_key TEXT NOT NULL UNIQUE CHECK (BTRIM(request_key) <> ''),
    target_kind TEXT NOT NULL CHECK (target_kind IN ('discord_session')),
    target_key TEXT NOT NULL CHECK (BTRIM(target_key) <> ''),
    requested_action TEXT NOT NULL CHECK (requested_action IN ('clear_for_resume')),
    state TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'fenced', 'applying', 'completed', 'aborted')),
    fence BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    aborted_at TIMESTAMPTZ,
    CHECK (
        (state = 'pending' AND fence IS NULL AND completed_at IS NULL AND aborted_at IS NULL)
        OR (state IN ('fenced', 'applying') AND fence IS NOT NULL AND fence > 0
            AND completed_at IS NULL AND aborted_at IS NULL)
        OR (state = 'completed' AND fence IS NOT NULL AND fence > 0
            AND completed_at IS NOT NULL AND aborted_at IS NULL)
        OR (state = 'aborted' AND completed_at IS NULL AND aborted_at IS NOT NULL)
    )
);

CREATE INDEX runtime_cleanup_operations_target_history_idx
    ON runtime_cleanup_operations (target_kind, target_key, created_at DESC);

CREATE UNIQUE INDEX runtime_cleanup_operations_one_open_target_idx
    ON runtime_cleanup_operations (target_kind, target_key)
    WHERE state IN ('pending', 'fenced', 'applying');

CREATE TABLE runtime_cleanup_intents (
    operation_id UUID NOT NULL REFERENCES runtime_cleanup_operations(operation_id) ON DELETE RESTRICT,
    intent_kind TEXT NOT NULL CHECK (
        intent_kind IN (
            'block_runtime_admission',
            'clear_queued_input',
            'expire_runtime_lease',
            'cancel_active_runtime',
            'clear_persisted_session',
            'release_runtime_slot'
        )
    ),
    ordinal SMALLINT NOT NULL CHECK (ordinal > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (operation_id, intent_kind),
    UNIQUE (operation_id, ordinal)
);

CREATE OR REPLACE FUNCTION agentdesk_reject_runtime_cleanup_intent_mutation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION USING
        ERRCODE = '55000',
        MESSAGE = 'runtime cleanup intents are immutable';
END;
$$;

CREATE TRIGGER trg_runtime_cleanup_intents_immutable
BEFORE UPDATE OR DELETE ON runtime_cleanup_intents
FOR EACH ROW
EXECUTE FUNCTION agentdesk_reject_runtime_cleanup_intent_mutation();

CREATE OR REPLACE FUNCTION agentdesk_guard_runtime_cleanup_operation_identity()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.operation_id IS DISTINCT FROM OLD.operation_id
        OR NEW.request_key IS DISTINCT FROM OLD.request_key
        OR NEW.target_kind IS DISTINCT FROM OLD.target_kind
        OR NEW.target_key IS DISTINCT FROM OLD.target_key
        OR NEW.requested_action IS DISTINCT FROM OLD.requested_action THEN
        RAISE EXCEPTION USING
            ERRCODE = '55000',
            MESSAGE = 'runtime cleanup operation identity is immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_runtime_cleanup_operation_identity_immutable
BEFORE UPDATE ON runtime_cleanup_operations
FOR EACH ROW
EXECUTE FUNCTION agentdesk_guard_runtime_cleanup_operation_identity();

CREATE TABLE runtime_cleanup_fences (
    target_kind TEXT NOT NULL CHECK (target_kind IN ('discord_session')),
    target_key TEXT NOT NULL CHECK (BTRIM(target_key) <> ''),
    last_fence BIGINT NOT NULL CHECK (last_fence > 0),
    operation_id UUID NOT NULL REFERENCES runtime_cleanup_operations(operation_id) ON DELETE RESTRICT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (target_kind, target_key),
    UNIQUE (operation_id),
    UNIQUE (target_kind, target_key, last_fence)
);
