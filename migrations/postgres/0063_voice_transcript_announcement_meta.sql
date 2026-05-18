-- Durable voice transcript announcement metadata.
--
-- Voice transcript announce messages intentionally keep the visible Discord
-- body human-readable only. The structured metadata needed to turn the
-- announce-bot message back into a voice turn therefore lives here instead of
-- in a hidden message suffix.
CREATE TABLE IF NOT EXISTS voice_transcript_announcement_meta (
    id                 BIGSERIAL PRIMARY KEY,
    pending_key        TEXT NOT NULL UNIQUE,
    message_id         TEXT,
    target_channel_id  TEXT NOT NULL,
    announce_content   TEXT NOT NULL,
    announcement       JSONB NOT NULL,
    consumed_at        TIMESTAMPTZ,
    bound_at           TIMESTAMPTZ,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS voice_transcript_announcement_meta_message_id_uq
    ON voice_transcript_announcement_meta (message_id)
    WHERE message_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_voice_transcript_announcement_meta_gc
    ON voice_transcript_announcement_meta (created_at);

CREATE OR REPLACE FUNCTION voice_transcript_announcement_meta_touch_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_voice_transcript_announcement_meta_touch_updated_at
    ON voice_transcript_announcement_meta;
CREATE TRIGGER trg_voice_transcript_announcement_meta_touch_updated_at
    BEFORE UPDATE ON voice_transcript_announcement_meta
    FOR EACH ROW EXECUTE FUNCTION voice_transcript_announcement_meta_touch_updated_at();
