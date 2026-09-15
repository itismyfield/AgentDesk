-- Live-channel recovery lease + append-only checkpoint WAL.
-- 0117 is reserved for auth-profiles rate_limit_cache.

CREATE TABLE agent_recovery_channel_state (
    channel_id TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    owner_agent_id TEXT NOT NULL,
    fallback_agent_id TEXT NOT NULL,
    active_writer_agent_id TEXT NOT NULL,
    workspace TEXT NOT NULL DEFAULT 'inherit',
    primary_turn_id TEXT,
    next_seq BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT chk_arcs_status CHECK (
        status IN (
            'owner',
            'fallback_running',
            'fallback_done',
            'restored',
            'aborted'
        )
    ),
    CONSTRAINT chk_arcs_workspace CHECK (workspace = 'inherit')
);

COMMENT ON TABLE agent_recovery_channel_state IS
    'Per-channel live recovery lease. YAML channel bindings are never rewritten.';

CREATE TABLE agent_recovery_checkpoint_events (
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL REFERENCES agent_recovery_channel_state (channel_id) ON DELETE CASCADE,
    seq BIGINT NOT NULL,
    at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    writer_agent_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload JSONB NOT NULL,
    payload_bytes INTEGER NOT NULL, -- agentdesk-audit: allow-int4 (bounded by routines.max_checkpoint_bytes)
    CONSTRAINT chk_arce_kind CHECK (
        kind IN (
            'owner_progress',
            'stall',
            'fallback_progress',
            'complete',
            'restore'
        )
    ),
    CONSTRAINT chk_arce_payload_bytes_nonneg CHECK (payload_bytes >= 0),
    CONSTRAINT uq_arce_channel_seq UNIQUE (channel_id, seq)
);

COMMENT ON TABLE agent_recovery_checkpoint_events IS
    'Append-only recovery WAL. Compact 5-section payload; no token stream.';

CREATE INDEX idx_arce_channel_seq_desc
    ON agent_recovery_checkpoint_events (channel_id, seq DESC);
