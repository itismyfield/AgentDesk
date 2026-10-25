CREATE TABLE provider_session_system_prompts (
    provider TEXT NOT NULL CHECK (provider IN ('claude', 'codex')),
    channel_id TEXT NOT NULL,
    provider_session_id TEXT NOT NULL,
    source_turn_id TEXT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL,
    system_prompt TEXT NOT NULL,
    anchor_count BIGINT NOT NULL CHECK (anchor_count >= 0),
    PRIMARY KEY (provider, channel_id, provider_session_id)
);
