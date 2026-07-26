-- Task #25 Slice 1: dormant durable runtime cleanup coordinator foundation.
--
-- These tables are additive journal/authority primitives only. No production
-- consumer reads or writes them until a later rollout slice activates the saga.

CREATE TABLE runtime_cleanup_operations (
    operation_id UUID PRIMARY KEY,
    request_key TEXT NOT NULL UNIQUE CHECK (BTRIM(request_key) <> ''),
    target_session_id BIGINT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
    requested_action TEXT NOT NULL CHECK (requested_action = 'clear_for_resume'),
    state TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'fenced', 'applying', 'completed', 'aborted')),
    fence BIGINT,
    claim_owner TEXT,
    attempt_token UUID,
    attempt_no BIGINT NOT NULL DEFAULT 0 CHECK (attempt_no >= 0),
    lease_expires_at TIMESTAMPTZ,
    attempt_started_at TIMESTAMPTZ,
    commit_decided_at TIMESTAMPTZ,
    aborted_from_state TEXT CHECK (aborted_from_state IN ('pending', 'fenced')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    aborted_at TIMESTAMPTZ,
    CHECK (
        (state = 'pending'
            AND fence IS NULL
            AND claim_owner IS NULL AND attempt_token IS NULL AND attempt_no = 0
            AND lease_expires_at IS NULL AND attempt_started_at IS NULL AND commit_decided_at IS NULL
            AND completed_at IS NULL AND aborted_at IS NULL AND aborted_from_state IS NULL)
        OR (state = 'fenced'
            AND fence IS NOT NULL AND fence > 0
            AND claim_owner IS NULL AND attempt_token IS NULL AND attempt_no = 0
            AND lease_expires_at IS NULL AND attempt_started_at IS NULL AND commit_decided_at IS NULL
            AND completed_at IS NULL AND aborted_at IS NULL AND aborted_from_state IS NULL)
        OR (state = 'applying'
            AND fence IS NOT NULL AND fence > 0
            AND claim_owner IS NOT NULL AND BTRIM(claim_owner) <> ''
            AND attempt_token IS NOT NULL AND attempt_no > 0
            AND lease_expires_at IS NOT NULL AND attempt_started_at IS NOT NULL
            AND lease_expires_at > attempt_started_at AND commit_decided_at IS NOT NULL
            AND completed_at IS NULL AND aborted_at IS NULL AND aborted_from_state IS NULL)
        OR (state = 'completed'
            AND fence IS NOT NULL AND fence > 0
            AND claim_owner IS NOT NULL AND BTRIM(claim_owner) <> ''
            AND attempt_token IS NOT NULL AND attempt_no > 0
            AND lease_expires_at IS NOT NULL AND attempt_started_at IS NOT NULL
            AND lease_expires_at > attempt_started_at AND commit_decided_at IS NOT NULL
            AND completed_at IS NOT NULL AND aborted_at IS NULL AND aborted_from_state IS NULL)
        OR (state = 'aborted'
            AND claim_owner IS NULL AND attempt_token IS NULL AND attempt_no = 0
            AND lease_expires_at IS NULL AND attempt_started_at IS NULL AND commit_decided_at IS NULL
            AND completed_at IS NULL AND aborted_at IS NOT NULL
            AND aborted_from_state IS NOT NULL
            AND ((aborted_from_state = 'pending' AND fence IS NULL)
                OR (aborted_from_state = 'fenced' AND fence IS NOT NULL AND fence > 0)))
    )
);

CREATE INDEX runtime_cleanup_operations_target_history_idx
    ON runtime_cleanup_operations (target_session_id, created_at DESC);

CREATE UNIQUE INDEX runtime_cleanup_operations_one_open_target_idx
    ON runtime_cleanup_operations (target_session_id)
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
    ordinal SMALLINT NOT NULL CHECK (ordinal BETWEEN 1 AND 6),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (operation_id, intent_kind),
    UNIQUE (operation_id, ordinal)
);

CREATE TABLE runtime_cleanup_operation_archive (
    operation_id UUID PRIMARY KEY,
    request_key TEXT NOT NULL UNIQUE,
    target_session_id BIGINT NOT NULL,
    requested_action TEXT NOT NULL,
    final_state TEXT NOT NULL CHECK (final_state IN ('completed', 'aborted')),
    fence BIGINT,
    final_attempt_no BIGINT NOT NULL,
    intents JSONB NOT NULL,
    retired_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

REVOKE ALL ON runtime_cleanup_operation_archive FROM PUBLIC;

CREATE OR REPLACE FUNCTION agentdesk_validate_runtime_cleanup_plan()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    checked_operation_id UUID;
    action_name TEXT;
    actual_plan TEXT[];
BEGIN
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;

    checked_operation_id := NEW.operation_id;
    SELECT requested_action INTO action_name
    FROM runtime_cleanup_operations
    WHERE operation_id = checked_operation_id;

    IF action_name IS NULL THEN
        RETURN NEW;
    END IF;

    SELECT ARRAY_AGG(intent_kind ORDER BY ordinal) INTO actual_plan
    FROM runtime_cleanup_intents
    WHERE operation_id = checked_operation_id;

    IF action_name = 'clear_for_resume' AND actual_plan IS DISTINCT FROM ARRAY[
        'block_runtime_admission',
        'clear_queued_input',
        'expire_runtime_lease',
        'cancel_active_runtime',
        'clear_persisted_session',
        'release_runtime_slot'
    ]::TEXT[] THEN
        RAISE EXCEPTION USING
            ERRCODE = '23514',
            CONSTRAINT = 'runtime_cleanup_intents_canonical_plan',
            MESSAGE = 'clear_for_resume requires the exact canonical six-step intent plan';
    END IF;

    RETURN NEW;
END;
$$;

CREATE CONSTRAINT TRIGGER trg_runtime_cleanup_operation_plan
AFTER INSERT OR UPDATE OF requested_action ON runtime_cleanup_operations
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION agentdesk_validate_runtime_cleanup_plan();

CREATE CONSTRAINT TRIGGER trg_runtime_cleanup_intent_plan
AFTER INSERT OR UPDATE OR DELETE ON runtime_cleanup_intents
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION agentdesk_validate_runtime_cleanup_plan();

CREATE OR REPLACE FUNCTION agentdesk_guard_runtime_cleanup_intent_mutation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'DELETE' AND EXISTS (
        SELECT 1
        FROM runtime_cleanup_operation_archive archive
        JOIN runtime_cleanup_operations operation USING (operation_id)
        WHERE archive.operation_id = OLD.operation_id
          AND operation.state IN ('completed', 'aborted')
          AND archive.final_state = operation.state
    ) THEN
        RETURN OLD;
    END IF;
    IF TG_OP <> 'INSERT' THEN
        RAISE EXCEPTION USING
            ERRCODE = '55000',
            MESSAGE = 'runtime cleanup intents are immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE OR REPLACE FUNCTION agentdesk_guard_runtime_cleanup_operation_transition()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.state = 'pending' AND NEW.state = 'fenced' THEN
        IF NEW.fence IS NULL OR NEW.fence <= 0 THEN
            RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'pending to fenced requires a positive fence';
        END IF;
    ELSIF OLD.state = 'pending' AND NEW.state = 'aborted' THEN
        IF NEW.aborted_from_state IS DISTINCT FROM 'pending' THEN
            RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'pending abort must preserve its origin';
        END IF;
    ELSIF OLD.state = 'fenced' AND NEW.state = 'applying' THEN
        IF NEW.fence IS DISTINCT FROM OLD.fence
            OR NEW.attempt_no <> 1
            OR NEW.attempt_started_at IS NULL
            OR NEW.lease_expires_at <= NEW.attempt_started_at THEN
            RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'fenced to applying requires the first live claimant';
        END IF;
    ELSIF OLD.state = 'fenced' AND NEW.state = 'aborted' THEN
        IF NEW.fence IS DISTINCT FROM OLD.fence
            OR NEW.aborted_from_state IS DISTINCT FROM 'fenced' THEN
            RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'fenced abort must preserve its fence and origin';
        END IF;
    ELSIF OLD.state = 'applying' AND NEW.state = 'applying' THEN
        IF NEW.attempt_started_at IS NULL
            OR OLD.lease_expires_at > NEW.attempt_started_at
            OR NEW.fence IS DISTINCT FROM OLD.fence
            OR NEW.attempt_no <> OLD.attempt_no + 1
            OR NEW.attempt_token IS NOT DISTINCT FROM OLD.attempt_token
            OR NEW.lease_expires_at <= NEW.attempt_started_at
            OR NEW.commit_decided_at IS DISTINCT FROM OLD.commit_decided_at THEN
            RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'applying takeover requires an expired lease and a new live attempt';
        END IF;
    ELSIF OLD.state = 'applying' AND NEW.state = 'completed' THEN
        IF NEW.fence IS DISTINCT FROM OLD.fence
            OR NEW.claim_owner IS DISTINCT FROM OLD.claim_owner
            OR NEW.attempt_token IS DISTINCT FROM OLD.attempt_token
            OR NEW.attempt_no IS DISTINCT FROM OLD.attempt_no
            OR NEW.lease_expires_at IS DISTINCT FROM OLD.lease_expires_at
            OR NEW.attempt_started_at IS DISTINCT FROM OLD.attempt_started_at
            OR NEW.commit_decided_at IS DISTINCT FROM OLD.commit_decided_at THEN
            RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'completion must preserve claimant authority';
        END IF;
    ELSE
        RAISE EXCEPTION USING
            ERRCODE = '55000',
            MESSAGE = FORMAT('illegal runtime cleanup transition: %s to %s', OLD.state, NEW.state);
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_runtime_cleanup_operation_transition
BEFORE UPDATE ON runtime_cleanup_operations
FOR EACH ROW
EXECUTE FUNCTION agentdesk_guard_runtime_cleanup_operation_transition();
CREATE TRIGGER trg_runtime_cleanup_intents_immutable
BEFORE UPDATE OR DELETE ON runtime_cleanup_intents
FOR EACH ROW
EXECUTE FUNCTION agentdesk_guard_runtime_cleanup_intent_mutation();

CREATE OR REPLACE FUNCTION agentdesk_guard_runtime_cleanup_operation_identity()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        IF EXISTS (
            SELECT 1
            FROM runtime_cleanup_operation_archive archive
            WHERE archive.operation_id = OLD.operation_id
              AND archive.request_key = OLD.request_key
              AND archive.target_session_id = OLD.target_session_id
              AND archive.requested_action = OLD.requested_action
              AND archive.final_state = OLD.state
              AND archive.fence IS NOT DISTINCT FROM OLD.fence
              AND archive.final_attempt_no = OLD.attempt_no
        ) THEN
            RETURN OLD;
        END IF;
        RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'runtime cleanup operations require terminal retention';
    END IF;
    IF NEW.operation_id IS DISTINCT FROM OLD.operation_id
        OR NEW.request_key IS DISTINCT FROM OLD.request_key
        OR NEW.target_session_id IS DISTINCT FROM OLD.target_session_id
        OR NEW.requested_action IS DISTINCT FROM OLD.requested_action THEN
        RAISE EXCEPTION USING
            ERRCODE = '55000',
            MESSAGE = 'runtime cleanup operation identity is immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_runtime_cleanup_operation_identity_immutable
BEFORE UPDATE OR DELETE ON runtime_cleanup_operations
FOR EACH ROW
EXECUTE FUNCTION agentdesk_guard_runtime_cleanup_operation_identity();

-- Fence authority deliberately keeps the last operation UUID after terminal
-- journal retention, so a later operation continues from the previous epoch.
CREATE TABLE runtime_cleanup_fences (
    target_session_id BIGINT PRIMARY KEY REFERENCES sessions(id) ON DELETE RESTRICT,
    last_fence BIGINT NOT NULL CHECK (last_fence > 0),
    operation_id UUID NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (operation_id),
    UNIQUE (target_session_id, last_fence)
);

CREATE OR REPLACE FUNCTION agentdesk_retire_terminal_runtime_cleanup_operation(
    retired_operation_id UUID,
    terminal_before TIMESTAMPTZ
)
RETURNS BOOLEAN
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
    retired_rows BIGINT;
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM public.runtime_cleanup_operations
        WHERE operation_id = retired_operation_id
          AND state IN ('completed', 'aborted')
          AND updated_at < terminal_before
        FOR UPDATE
    ) THEN
        RETURN FALSE;
    END IF;

    INSERT INTO public.runtime_cleanup_operation_archive (
        operation_id, request_key, target_session_id, requested_action,
        final_state, fence, final_attempt_no, intents
    )
    SELECT
        operation.operation_id,
        operation.request_key,
        operation.target_session_id,
        operation.requested_action,
        operation.state,
        operation.fence,
        operation.attempt_no,
        COALESCE(
            JSONB_AGG(
                JSONB_BUILD_OBJECT('kind', intent.intent_kind, 'ordinal', intent.ordinal)
                ORDER BY intent.ordinal
            ) FILTER (WHERE intent.operation_id IS NOT NULL),
            '[]'::JSONB
        )
    FROM public.runtime_cleanup_operations operation
    LEFT JOIN public.runtime_cleanup_intents intent USING (operation_id)
    WHERE operation.operation_id = retired_operation_id
    GROUP BY operation.operation_id;

    DELETE FROM public.runtime_cleanup_intents WHERE operation_id = retired_operation_id;
    DELETE FROM public.runtime_cleanup_operations WHERE operation_id = retired_operation_id;
    GET DIAGNOSTICS retired_rows = ROW_COUNT;
    RETURN retired_rows = 1;
END;
$$;

REVOKE ALL ON FUNCTION agentdesk_retire_terminal_runtime_cleanup_operation(UUID, TIMESTAMPTZ) FROM PUBLIC;
