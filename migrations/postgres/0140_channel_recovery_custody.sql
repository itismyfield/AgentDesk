-- One recovery-consumption custody row per channel, shared by every node. A row changes only by
-- revision CAS and is never deleted; no home row reference or cascade reaches it.
CREATE TABLE IF NOT EXISTS channel_recovery_custody (
    channel_id TEXT PRIMARY KEY CHECK (channel_id ~ '^[1-9][0-9]*$'),
    revision BIGINT NOT NULL CHECK (revision >= 1),
    provider TEXT NOT NULL CHECK (BTRIM(provider) <> ''),
    token_hash TEXT NOT NULL CHECK (BTRIM(token_hash) <> ''),
    owner_node TEXT NOT NULL CHECK (BTRIM(owner_node) <> ''),
    process_incarnation TEXT NOT NULL CHECK (BTRIM(process_incarnation) <> ''),
    attempt_id TEXT NOT NULL CHECK (BTRIM(attempt_id) <> ''),
    -- NULL is an explicit absence (a no-home legacy channel), never a placeholder epoch.
    home_epoch BIGINT CHECK (home_epoch >= 1),
    activation_generation BIGINT CHECK (activation_generation >= 0),
    issued_episode JSONB CHECK (jsonb_typeof(issued_episode) = 'object'),
    -- Phase and JSON shapes belong to format_version; a reader that does not know them holds.
    phase TEXT NOT NULL CHECK (BTRIM(phase) <> ''),
    active_operation JSONB CHECK (jsonb_typeof(active_operation) = 'object'),
    canonical_seal JSONB NOT NULL CHECK (jsonb_typeof(canonical_seal) = 'object'),
    ever_scoped BOOLEAN NOT NULL DEFAULT FALSE,
    format_version INTEGER NOT NULL CHECK (format_version >= 1), -- agentdesk-audit: allow-int4 (row format version)
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Bounded progress only: no message bodies and no operation history.
    CONSTRAINT channel_recovery_custody_bounded CHECK (
        octet_length(COALESCE(issued_episode::text, '')) <= 4096
        AND octet_length(COALESCE(active_operation::text, '')) <= 16384
        AND octet_length(canonical_seal::text) <= 4096
    )
);

-- Rows that still hold a responsibility, for restart reconstruction.
CREATE INDEX IF NOT EXISTS channel_recovery_custody_open
    ON channel_recovery_custody (channel_id) WHERE phase <> 'closed';
