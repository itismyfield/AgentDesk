CREATE TABLE intake_attachment_bundles (
    id UUID PRIMARY KEY,
    provider TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    user_msg_id TEXT NOT NULL,
    manifest JSONB NOT NULL CHECK (jsonb_typeof(manifest) = 'array' AND jsonb_array_length(manifest) BETWEEN 1 AND 10),
    payload BYTEA NOT NULL CHECK (octet_length(payload) <= 25165824),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL DEFAULT NOW() + INTERVAL '24 hours',
    UNIQUE (provider, channel_id, user_msg_id)
);
CREATE INDEX intake_attachment_bundles_expiry ON intake_attachment_bundles (expires_at);
ALTER TABLE intake_outbox ADD COLUMN attachment_refs JSONB NOT NULL DEFAULT '[]'
    CHECK (jsonb_typeof(attachment_refs) = 'array');
