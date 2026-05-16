-- Durable metadata for visible voice transcript announcement messages.
--
-- The Discord message body intentionally no longer carries hidden
-- transcript metadata. This table lets a different intake process
-- reconstruct the canonical VoiceTranscriptAnnouncement from the
-- announce-bot message_id.
CREATE TABLE IF NOT EXISTS voice_transcript_announce_meta (
    message_id TEXT PRIMARY KEY,
    announcement JSONB NOT NULL,
    consumed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_voice_transcript_announce_meta_created_at
    ON voice_transcript_announce_meta (created_at);

CREATE OR REPLACE FUNCTION voice_transcript_announce_meta_touch_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_voice_transcript_announce_meta_touch_updated_at
    ON voice_transcript_announce_meta;
CREATE TRIGGER trg_voice_transcript_announce_meta_touch_updated_at
    BEFORE UPDATE ON voice_transcript_announce_meta
    FOR EACH ROW EXECUTE FUNCTION voice_transcript_announce_meta_touch_updated_at();
