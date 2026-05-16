-- Durable persistence for voice-background handoff markers (#2274).
--
-- #2273 (#2236) introduced a typed in-memory `VoiceBackgroundHandoffMeta`
-- store that binds a posted prompt's `message_id` to its originating voice
-- channel. Long-running background turns can take minutes to terminally
-- deliver, so a dcserver restart during a turn loses the marker — the
-- spoken summary is dropped (fail-safe, not mis-routed), but undesirable
-- for resilience.
--
-- Mirrors the durability pattern of `voice_transcript_announce_meta`
-- (#2245): a side store keyed by `message_id`, GC'd by the leader-only
-- maintenance scheduler. TTL is much longer here (1 hour vs 10 min) to
-- accommodate legitimate long background turns.
CREATE TABLE IF NOT EXISTS voice_background_handoff_meta (
    message_id TEXT PRIMARY KEY,
    voice_channel_id TEXT NOT NULL,
    background_channel_id TEXT NOT NULL,
    agent_id TEXT,
    consumed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_voice_background_handoff_meta_created_at
    ON voice_background_handoff_meta (created_at);

CREATE OR REPLACE FUNCTION voice_background_handoff_meta_touch_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_voice_background_handoff_meta_touch_updated_at
    ON voice_background_handoff_meta;
CREATE TRIGGER trg_voice_background_handoff_meta_touch_updated_at
    BEFORE UPDATE ON voice_background_handoff_meta
    FOR EACH ROW EXECUTE FUNCTION voice_background_handoff_meta_touch_updated_at();
