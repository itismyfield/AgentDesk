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
      AND session_key IS NOT NULL
      AND session_key LIKE '%/%/%:%'
      AND split_part(session_key, '/', 1) = provider
      AND split_part(session_key, '/', 2) ~ '^discord_[0-9a-f]{16}$'
      -- A generated `scheduled:{definition_id}` basis sanitizes to a tmux tail
      -- beginning with `scheduled-`; keep every such legacy row unclassified.
      AND reverse(split_part(reverse(session_key), ':', 1))
          !~ '^AgentDesk-[^-]+-scheduled-'
)
UPDATE sessions AS target
SET identity_kind = 'discord_channel',
    discord_token_hash = eligible.token_hash
FROM eligible
WHERE target.id = eligible.id
  AND eligible.tuple_count = 1;
