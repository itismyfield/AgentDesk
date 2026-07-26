-- Task #25 replacement: dormant PostgreSQL cleanup substrate only.
--
-- No production consumer or external-side-effect authority is activated by this
-- migration. Database epochs and capabilities reject stale database mutations;
-- they cannot prove that an external destination fenced an already issued side
-- effect. Future consumers must enforce target high-watermarks at destinations.

CREATE TABLE runtime_cleanup_targets (
    target_id UUID PRIMARY KEY,
    identity_kind TEXT NOT NULL CHECK (identity_kind IN ('discord_channel', 'scheduled_snapshot')),
    provider TEXT NOT NULL CHECK (BTRIM(provider) <> ''),
    discord_token_hash TEXT NOT NULL CHECK (discord_token_hash ~ '^discord_[0-9a-f]{16}$'),
    channel_id TEXT NOT NULL CHECK (BTRIM(channel_id) <> ''),
    operation_high_watermark BIGINT NOT NULL DEFAULT 0 CHECK (operation_high_watermark >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    retired_at TIMESTAMPTZ,
    UNIQUE (identity_kind, provider, discord_token_hash, channel_id)
);

CREATE TABLE runtime_cleanup_target_session_bindings (
    session_id BIGINT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    target_id UUID NOT NULL REFERENCES runtime_cleanup_targets(target_id) ON DELETE RESTRICT,
    bound_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX runtime_cleanup_target_session_bindings_target_idx
    ON runtime_cleanup_target_session_bindings (target_id);

-- A locator history row is never deleted or reused. active=false is the retired
-- generation watermark; a later claim receives max(generation)+1.
CREATE TABLE runtime_cleanup_locator_claims (
    locator TEXT NOT NULL CHECK (BTRIM(locator) <> ''),
    generation BIGINT NOT NULL CHECK (generation > 0),
    target_id UUID NOT NULL REFERENCES runtime_cleanup_targets(target_id) ON DELETE RESTRICT,
    active BOOLEAN NOT NULL DEFAULT TRUE,
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    retired_at TIMESTAMPTZ,
    PRIMARY KEY (locator, generation),
    CHECK ((active AND retired_at IS NULL) OR (NOT active AND retired_at IS NOT NULL))
);
CREATE UNIQUE INDEX runtime_cleanup_locator_claims_one_active_idx
    ON runtime_cleanup_locator_claims (locator) WHERE active;

CREATE OR REPLACE FUNCTION agentdesk_guard_runtime_cleanup_locator_claim()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'runtime cleanup locator generations are permanent authority';
    END IF;
    IF NEW.locator IS DISTINCT FROM OLD.locator
       OR NEW.generation IS DISTINCT FROM OLD.generation
       OR NEW.target_id IS DISTINCT FROM OLD.target_id
       OR NEW.claimed_at IS DISTINCT FROM OLD.claimed_at
       OR NOT (OLD.active AND NOT NEW.active AND OLD.retired_at IS NULL AND NEW.retired_at IS NOT NULL) THEN
        RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'runtime cleanup locator generations are immutable except retirement';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER trg_runtime_cleanup_locator_claim_guard
BEFORE UPDATE OR DELETE ON runtime_cleanup_locator_claims
FOR EACH ROW EXECUTE FUNCTION agentdesk_guard_runtime_cleanup_locator_claim();

CREATE TABLE runtime_cleanup_operations (
    operation_id UUID PRIMARY KEY,
    target_id UUID NOT NULL REFERENCES runtime_cleanup_targets(target_id) ON DELETE RESTRICT,
    operation_epoch BIGINT NOT NULL CHECK (operation_epoch > 0),
    operation_kind TEXT NOT NULL CHECK (operation_kind = 'clear_for_resume'),
    state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'committed', 'completed', 'aborted')),
    claim_owner TEXT,
    attempt_epoch BIGINT NOT NULL DEFAULT 0 CHECK (attempt_epoch >= 0),
    claim_expires_at TIMESTAMPTZ,
    committed_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    aborted_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (target_id, operation_epoch),
    UNIQUE (operation_id, target_id),
    CHECK (
        (state = 'open' AND committed_at IS NULL AND completed_at IS NULL AND aborted_at IS NULL)
        OR (state = 'committed' AND committed_at IS NOT NULL AND completed_at IS NULL AND aborted_at IS NULL)
        OR (state = 'completed' AND committed_at IS NOT NULL AND completed_at IS NOT NULL AND aborted_at IS NULL)
        OR (state = 'aborted' AND committed_at IS NULL AND completed_at IS NULL AND aborted_at IS NOT NULL)
    ),
    CHECK (
        (claim_owner IS NULL AND claim_expires_at IS NULL)
        OR (claim_owner IS NOT NULL AND BTRIM(claim_owner) <> '' AND claim_expires_at IS NOT NULL AND attempt_epoch > 0)
    )
);
CREATE UNIQUE INDEX runtime_cleanup_operations_one_open_target_idx
    ON runtime_cleanup_operations (target_id) WHERE state IN ('open', 'committed');

CREATE TABLE runtime_cleanup_intents (
    operation_id UUID NOT NULL REFERENCES runtime_cleanup_operations(operation_id) ON DELETE RESTRICT,
    intent_id UUID NOT NULL,
    ordinal SMALLINT NOT NULL CHECK (ordinal > 0),
    intent_kind TEXT NOT NULL CHECK (intent_kind IN (
        'block_runtime_admission', 'clear_queued_input', 'expire_runtime_lease',
        'cancel_active_runtime', 'clear_persisted_session', 'release_runtime_slot'
    )),
    target_id UUID NOT NULL REFERENCES runtime_cleanup_targets(target_id) ON DELETE RESTRICT,
    idempotency_identity UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (operation_id, intent_id),
    UNIQUE (operation_id, ordinal),
    UNIQUE (operation_id, idempotency_identity),
    UNIQUE (operation_id, intent_id, target_id, idempotency_identity),
    FOREIGN KEY (operation_id, target_id)
        REFERENCES runtime_cleanup_operations(operation_id, target_id) ON DELETE RESTRICT,
    CHECK ((ordinal, intent_kind) IN (
        (1, 'block_runtime_admission'),
        (2, 'clear_queued_input'),
        (3, 'expire_runtime_lease'),
        (4, 'cancel_active_runtime'),
        (5, 'clear_persisted_session'),
        (6, 'release_runtime_slot')
    ))
);

-- The plaintext capability is returned once by the API. Only its SHA-256 digest
-- is stored. All binding fields are exact typed columns, never hash-only identity.
CREATE OR REPLACE FUNCTION agentdesk_require_runtime_cleanup_plan()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE plan_is_canonical BOOLEAN;
BEGIN
    SELECT COUNT(*) = 6
       AND BOOL_AND((ordinal, intent_kind) IN (
           (1, 'block_runtime_admission'),
           (2, 'clear_queued_input'),
           (3, 'expire_runtime_lease'),
           (4, 'cancel_active_runtime'),
           (5, 'clear_persisted_session'),
           (6, 'release_runtime_slot')
       ))
    INTO plan_is_canonical
    FROM runtime_cleanup_intents
    WHERE operation_id = NEW.operation_id;
    IF NOT plan_is_canonical THEN
        RAISE EXCEPTION USING ERRCODE = '23514', MESSAGE = 'runtime cleanup operation requires the canonical six-intent plan';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER trg_runtime_cleanup_operation_plan
AFTER INSERT ON runtime_cleanup_operations
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION agentdesk_require_runtime_cleanup_plan();

CREATE TABLE runtime_cleanup_capabilities (
    capability_id UUID PRIMARY KEY,
    capability_hash BYTEA NOT NULL UNIQUE CHECK (OCTET_LENGTH(capability_hash) = 32),
    target_id UUID NOT NULL REFERENCES runtime_cleanup_targets(target_id) ON DELETE RESTRICT,
    operation_id UUID NOT NULL REFERENCES runtime_cleanup_operations(operation_id) ON DELETE RESTRICT,
    intent_id UUID NOT NULL,
    attempt_epoch BIGINT NOT NULL CHECK (attempt_epoch > 0),
    audience TEXT NOT NULL CHECK (BTRIM(audience) <> ''),
    expires_at TIMESTAMPTZ NOT NULL,
    idempotency_identity UUID NOT NULL,
    consumed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    FOREIGN KEY (operation_id, intent_id, target_id, idempotency_identity)
        REFERENCES runtime_cleanup_intents(operation_id, intent_id, target_id, idempotency_identity)
        ON DELETE RESTRICT,
    UNIQUE (operation_id, intent_id, attempt_epoch, audience, idempotency_identity)
);

-- request_id is server-issued and permanent. Payload/result retention is kept in
-- a separate row so 30-day GC can never make the UUID reusable.
CREATE TABLE runtime_cleanup_request_identities (
    request_id UUID PRIMARY KEY,
    operation_id UUID NOT NULL REFERENCES runtime_cleanup_operations(operation_id) ON DELETE RESTRICT,
    intent_id UUID NOT NULL,
    idempotency_identity UUID NOT NULL,
    request_fingerprint BYTEA NOT NULL CHECK (OCTET_LENGTH(request_fingerprint) = 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    FOREIGN KEY (operation_id, intent_id)
        REFERENCES runtime_cleanup_intents(operation_id, intent_id) ON DELETE RESTRICT,
    UNIQUE (operation_id, intent_id, idempotency_identity)
);

CREATE TABLE runtime_cleanup_receipts (
    request_id UUID PRIMARY KEY REFERENCES runtime_cleanup_request_identities(request_id) ON DELETE RESTRICT,
    receipt_state TEXT NOT NULL CHECK (receipt_state IN ('applied', 'not_applied', 'unknown')),
    result_fingerprint BYTEA CHECK (result_fingerprint IS NULL OR OCTET_LENGTH(result_fingerprint) = 32),
    terminal_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    retain_until TIMESTAMPTZ NOT NULL,
    CHECK (retain_until >= terminal_at + INTERVAL '30 days')
);

CREATE OR REPLACE FUNCTION agentdesk_guard_runtime_cleanup_operation()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'runtime cleanup operations are permanent authority';
    END IF;
    IF NEW.operation_id IS DISTINCT FROM OLD.operation_id
       OR NEW.target_id IS DISTINCT FROM OLD.target_id
       OR NEW.operation_epoch IS DISTINCT FROM OLD.operation_epoch
       OR NEW.operation_kind IS DISTINCT FROM OLD.operation_kind
       OR NEW.attempt_epoch < OLD.attempt_epoch THEN
        RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = 'runtime cleanup operation identity and epochs are immutable or monotonic';
    END IF;
    IF NOT ((OLD.state = 'open' AND NEW.state IN ('open', 'committed', 'aborted'))
         OR (OLD.state = 'committed' AND NEW.state IN ('committed', 'completed'))
         OR (OLD.state = 'completed' AND NEW.state = 'completed')
         OR (OLD.state = 'aborted' AND NEW.state = 'aborted')) THEN
        RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = FORMAT('illegal runtime cleanup transition: %s to %s', OLD.state, NEW.state);
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER trg_runtime_cleanup_operation_guard
BEFORE UPDATE OR DELETE ON runtime_cleanup_operations
FOR EACH ROW EXECUTE FUNCTION agentdesk_guard_runtime_cleanup_operation();

CREATE OR REPLACE FUNCTION agentdesk_guard_runtime_cleanup_immutable()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION USING ERRCODE = '55000', MESSAGE = FORMAT('%s rows are immutable', TG_TABLE_NAME);
END;
$$;
CREATE TRIGGER trg_runtime_cleanup_intents_immutable
BEFORE UPDATE OR DELETE ON runtime_cleanup_intents
FOR EACH ROW EXECUTE FUNCTION agentdesk_guard_runtime_cleanup_immutable();
CREATE TRIGGER trg_runtime_cleanup_request_identities_immutable
BEFORE UPDATE OR DELETE ON runtime_cleanup_request_identities
FOR EACH ROW EXECUTE FUNCTION agentdesk_guard_runtime_cleanup_immutable();

-- Explicit lock order for every API transaction:
-- request UUID advisory lock -> canonical identity advisory lock -> sorted locator
-- locks via agentdesk_lock_session_locator -> target row -> operation row -> intent
-- row -> capability/request/receipt row. Callers must never acquire in reverse.

CREATE OR REPLACE FUNCTION agentdesk_gc_runtime_cleanup_receipts(batch_size INTEGER)
RETURNS INTEGER LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE deleted_count INTEGER;
BEGIN
    IF batch_size < 1 OR batch_size > 1000 THEN
        RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'batch_size must be between 1 and 1000';
    END IF;
    WITH doomed AS (
        SELECT request_id FROM public.runtime_cleanup_receipts
        WHERE retain_until <= clock_timestamp()
        ORDER BY retain_until, request_id
        FOR UPDATE SKIP LOCKED LIMIT batch_size
    )
    DELETE FROM public.runtime_cleanup_receipts receipt
    USING doomed WHERE receipt.request_id = doomed.request_id;
    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    RETURN deleted_count;
END;
$$;
REVOKE ALL ON FUNCTION agentdesk_gc_runtime_cleanup_receipts(INTEGER) FROM PUBLIC;
