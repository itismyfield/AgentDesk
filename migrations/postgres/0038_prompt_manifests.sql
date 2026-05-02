CREATE TABLE IF NOT EXISTS prompt_manifests (
    id                     BIGSERIAL PRIMARY KEY,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    turn_id                TEXT NOT NULL,
    channel_id             TEXT NOT NULL,
    dispatch_id            TEXT,
    profile                TEXT,
    total_input_tokens_est BIGINT NOT NULL DEFAULT 0,
    layer_count            BIGINT NOT NULL DEFAULT 0
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_prompt_manifests_turn_id
    ON prompt_manifests(turn_id);

CREATE INDEX IF NOT EXISTS idx_prompt_manifests_channel_id
    ON prompt_manifests(channel_id);

CREATE INDEX IF NOT EXISTS idx_prompt_manifests_created_at
    ON prompt_manifests(created_at DESC);

CREATE TABLE IF NOT EXISTS prompt_manifest_layers (
    id                 BIGSERIAL PRIMARY KEY,
    manifest_id        BIGINT NOT NULL REFERENCES prompt_manifests(id) ON DELETE CASCADE,
    layer_name         TEXT NOT NULL,
    enabled            BOOLEAN NOT NULL,
    source             TEXT,
    reason             TEXT,
    chars              BIGINT NOT NULL DEFAULT 0,
    tokens_est         BIGINT NOT NULL DEFAULT 0,
    content_sha256     TEXT NOT NULL,
    content_visibility TEXT NOT NULL,
    full_content       TEXT,
    redacted_preview   TEXT,
    CONSTRAINT prompt_manifest_layers_content_visibility_check
        CHECK (content_visibility IN ('adk_provided', 'user_derived'))
);

CREATE INDEX IF NOT EXISTS idx_prompt_manifest_layers_manifest_id
    ON prompt_manifest_layers(manifest_id);
