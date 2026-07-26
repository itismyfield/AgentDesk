-- #4913 GO-A1: additive canonical Discord channel identity and locator aliases.
--
-- `sessions.session_key` remains the current tmux/host locator. These nullable
-- columns carry the semantic Discord owner without forcing old binaries to send
-- the new fields.
ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS identity_kind TEXT,
    ADD COLUMN IF NOT EXISTS discord_token_hash TEXT;

ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_identity_kind_check;
ALTER TABLE sessions
    ADD CONSTRAINT sessions_identity_kind_check
    CHECK (identity_kind IS NULL OR identity_kind IN ('discord_channel', 'scheduled_snapshot'));

-- Previous locators stay attached to the durable sessions.id row. Keeping the
-- alias primary key global makes a locator-to-two-rows collision impossible.
CREATE TABLE IF NOT EXISTS session_key_aliases (
    session_key TEXT PRIMARY KEY,
    session_id BIGINT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS session_key_aliases_session_id_idx
    ON session_key_aliases (session_id);

-- A locator has one cross-table namespace owner. The unique claim row is the
-- race-safe authority for old binaries too: unlike a trigger-side EXISTS check,
-- its unique-index conflict remains correct when two statements took snapshots
-- before either transaction committed.
CREATE TABLE IF NOT EXISTS session_locator_namespace (
    session_key TEXT PRIMARY KEY,
    owner_kind TEXT NOT NULL CHECK (owner_kind IN ('primary', 'alias'))
);

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM sessions s
        JOIN session_key_aliases a ON a.session_key = s.session_key
    ) THEN
        RAISE EXCEPTION USING
            ERRCODE = '23505',
            CONSTRAINT = 'session_locator_namespace',
            MESSAGE = 'session locator exists as both a primary row and an alias';
    END IF;
END;
$$;

INSERT INTO session_locator_namespace (session_key, owner_kind)
SELECT session_key, 'primary'
FROM sessions
WHERE session_key IS NOT NULL
ON CONFLICT (session_key) DO NOTHING;

INSERT INTO session_locator_namespace (session_key, owner_kind)
SELECT session_key, 'alias'
FROM session_key_aliases
ON CONFLICT (session_key) DO NOTHING;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM sessions s
        JOIN session_locator_namespace n USING (session_key)
        WHERE n.owner_kind <> 'primary'
    ) OR EXISTS (
        SELECT 1
        FROM session_key_aliases a
        JOIN session_locator_namespace n USING (session_key)
        WHERE n.owner_kind <> 'alias'
    ) THEN
        RAISE EXCEPTION USING
            ERRCODE = '23505',
            CONSTRAINT = 'session_locator_namespace',
            MESSAGE = 'session locator namespace claim conflicts with durable ownership';
    END IF;
END;
$$;

-- One database-owned lock key is shared by new binaries and namespace triggers.
-- The claim table supplies the hard invariant; the lock keeps current binaries'
-- evidence reads and writes in one deterministic locator order.
CREATE OR REPLACE FUNCTION agentdesk_lock_session_locator(locator TEXT)
RETURNS VOID
LANGUAGE plpgsql
AS $$
BEGIN
    IF locator IS NULL OR BTRIM(locator) = '' THEN
        RETURN;
    END IF;
    PERFORM pg_advisory_xact_lock(4913, hashtext('locator:' || locator));
END;
$$;

CREATE OR REPLACE FUNCTION agentdesk_guard_session_locator_namespace()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    expected_kind TEXT;
    claimed_kind TEXT;
BEGIN
    IF NEW.session_key IS NULL THEN
        RETURN NEW;
    END IF;

    PERFORM agentdesk_lock_session_locator(NEW.session_key);
    expected_kind := CASE WHEN TG_TABLE_NAME = 'sessions' THEN 'primary' ELSE 'alias' END;

    INSERT INTO session_locator_namespace (session_key, owner_kind)
    VALUES (NEW.session_key, expected_kind)
    ON CONFLICT (session_key) DO UPDATE
    SET owner_kind = session_locator_namespace.owner_kind
    WHERE session_locator_namespace.owner_kind = EXCLUDED.owner_kind
    RETURNING owner_kind INTO claimed_kind;

    IF claimed_kind IS NULL THEN
        RAISE EXCEPTION USING
            ERRCODE = '23505',
            CONSTRAINT = 'session_locator_namespace',
            MESSAGE = 'session locator already belongs to the other namespace';
    END IF;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION agentdesk_release_session_locator_namespace()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    expected_kind TEXT;
BEGIN
    IF TG_OP = 'UPDATE' AND OLD.session_key IS NOT DISTINCT FROM NEW.session_key THEN
        RETURN NEW;
    END IF;

    expected_kind := CASE WHEN TG_TABLE_NAME = 'sessions' THEN 'primary' ELSE 'alias' END;
    DELETE FROM session_locator_namespace
    WHERE session_key = OLD.session_key
      AND owner_kind = expected_kind;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS trg_sessions_locator_namespace ON sessions;
CREATE TRIGGER trg_sessions_locator_namespace
BEFORE INSERT OR UPDATE OF session_key ON sessions
FOR EACH ROW
EXECUTE FUNCTION agentdesk_guard_session_locator_namespace();

DROP TRIGGER IF EXISTS trg_sessions_locator_namespace_release ON sessions;
CREATE TRIGGER trg_sessions_locator_namespace_release
AFTER DELETE OR UPDATE OF session_key ON sessions
FOR EACH ROW
EXECUTE FUNCTION agentdesk_release_session_locator_namespace();

DROP TRIGGER IF EXISTS trg_session_key_aliases_locator_namespace ON session_key_aliases;
CREATE TRIGGER trg_session_key_aliases_locator_namespace
BEFORE INSERT OR UPDATE OF session_key ON session_key_aliases
FOR EACH ROW
EXECUTE FUNCTION agentdesk_guard_session_locator_namespace();

DROP TRIGGER IF EXISTS trg_session_key_aliases_locator_namespace_release ON session_key_aliases;
CREATE TRIGGER trg_session_key_aliases_locator_namespace_release
AFTER DELETE OR UPDATE OF session_key ON session_key_aliases
FOR EACH ROW
EXECUTE FUNCTION agentdesk_release_session_locator_namespace();

-- Migration and runtime promotion call this same classifier. It deliberately
-- fails closed for any generated scheduled-snapshot tmux prefix and validates
-- the namespaced provider/token owner without duplicating naming heuristics in
-- Rust. A real legacy channel whose sanitized name shares the reserved prefix
-- remains unclassified rather than risking incorrect ownership inference.
CREATE OR REPLACE FUNCTION agentdesk_legacy_discord_locator_is_ordinary(
    locator TEXT,
    expected_provider TEXT,
    expected_token_hash TEXT
)
RETURNS BOOLEAN
LANGUAGE SQL
IMMUTABLE
PARALLEL SAFE
AS $$
    SELECT COALESCE(
        locator IS NOT NULL
        AND expected_provider IS NOT NULL
        AND BTRIM(expected_provider) <> ''
        AND expected_token_hash ~ '^discord_[0-9a-f]{16}$'
        AND split_part(locator, '/', 1) = expected_provider
        AND split_part(locator, '/', 2) = expected_token_hash
        AND split_part(locator, '/', 3) <> ''
        AND split_part(locator, '/', 4) = ''
        AND STRPOS(split_part(locator, '/', 3), ':') > 1
        AND LEFT(
            reverse(split_part(reverse(locator), ':', 1)),
            CHAR_LENGTH('AgentDesk-' || expected_provider || '-scheduled-')
        ) <> 'AgentDesk-' || expected_provider || '-scheduled-',
        FALSE
    );
$$;

-- Only complete ordinary Discord identities participate. Scheduled snapshots
-- deliberately remain outside this authority even when they carry source-channel
-- metadata, and nullable legacy rows remain valid for mixed-version operation.
CREATE UNIQUE INDEX IF NOT EXISTS sessions_canonical_discord_identity_uidx
    ON sessions (provider, discord_token_hash, channel_id)
    WHERE identity_kind = 'discord_channel'
      AND provider IS NOT NULL AND BTRIM(provider) <> ''
      AND discord_token_hash IS NOT NULL AND BTRIM(discord_token_hash) <> ''
      AND channel_id IS NOT NULL AND BTRIM(channel_id) <> '';

-- Conservative legacy backfill. Ownership is promoted only when the provider
-- and token namespace encoded in the current namespaced locator agree with the
-- row and exactly one eligible row owns the tuple. Scheduled snapshot locators
-- are excluded explicitly; duplicate tuples and all unparsable/null rows remain
-- untouched for typed runtime conflict handling.
WITH eligible AS (
    SELECT
        id,
        provider,
        split_part(session_key, '/', 2) AS token_hash,
        channel_id,
        COUNT(*) OVER (
            PARTITION BY provider, split_part(session_key, '/', 2), channel_id
        ) AS tuple_count
    FROM sessions
    WHERE identity_kind IS NULL
      AND discord_token_hash IS NULL
      AND provider IS NOT NULL AND BTRIM(provider) <> ''
      AND channel_id IS NOT NULL AND BTRIM(channel_id) <> ''
      AND agentdesk_legacy_discord_locator_is_ordinary(
          session_key,
          provider,
          split_part(session_key, '/', 2)
      )
)
UPDATE sessions AS target
SET identity_kind = 'discord_channel',
    discord_token_hash = eligible.token_hash
FROM eligible
WHERE target.id = eligible.id
  AND eligible.tuple_count = 1;
