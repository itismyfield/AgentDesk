CREATE INDEX IF NOT EXISTS idx_session_transcripts_channel_created
    ON session_transcripts(channel_id, created_at DESC);
