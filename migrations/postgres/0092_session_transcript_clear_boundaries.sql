DROP INDEX IF EXISTS idx_session_transcripts_channel_created;

CREATE INDEX idx_session_transcripts_channel_created
    ON session_transcripts(channel_id, created_at DESC, id DESC);

CREATE TABLE channel_session_clear_boundaries (
    channel_id TEXT PRIMARY KEY,
    cleared_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
