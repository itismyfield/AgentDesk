CREATE TABLE IF NOT EXISTS agent_archive (
    agent_id               TEXT PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
    state                  TEXT NOT NULL DEFAULT 'archived',
    reason                 TEXT,
    previous_status        TEXT,
    config_agent_json      JSONB,
    role_map_snapshot_json JSONB,
    prompt_path            TEXT,
    discord_channels_json  JSONB,
    discord_action         TEXT,
    discord_result_json    JSONB,
    archived_at            TIMESTAMPTZ,
    unarchived_at          TIMESTAMPTZ,
    updated_at             TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_agent_archive_state
    ON agent_archive(state);
