-- #2362 / #2164 Voice A — VoiceTurnLink durable store.
--
-- Canonical bridge between a voice channel and the background text channel
-- that owns a routed voice turn. The link survives process restarts and
-- powers reverse lookups for:
--
--   * Final TTS playback target resolution (voice_channel_id from
--     dispatch_id; #2164 C6).
--   * Barge-in cancel routing — given the source voice channel resolve the
--     active background target (#2164 C7).
--   * agent:done audible feedback routing (#2164 C8).
--
-- Lifecycle:
--   1. Insert at voice → background dispatch creation. status='active'.
--   2. Retarget (different background channel for the same utterance):
--      previous generation is marked 'cancelled', a new row with
--      generation+1 is inserted as 'active'. Simple retries colliding on
--      the same (guild, voice_channel, utterance, generation) tuple are
--      deduped via ON CONFLICT DO NOTHING.
--   3. Terminal delivery (TTS finished, run_completed, etc.) sets
--      status='terminal'. Terminal rows are eventually GC'd by the
--      leader-only maintenance scheduler.

CREATE TABLE IF NOT EXISTS voice_turn_link (
    id                     BIGSERIAL PRIMARY KEY,
    guild_id               BIGINT NOT NULL,
    voice_channel_id       BIGINT NOT NULL,
    background_channel_id  BIGINT NOT NULL,
    utterance_id           TEXT   NOT NULL,
    generation             INTEGER NOT NULL,
    announce_message_id    BIGINT,
    dispatch_id            TEXT,
    status                 TEXT   NOT NULL,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT voice_turn_link_status_check
        CHECK (status IN ('active', 'cancelled', 'terminal')),
    CONSTRAINT voice_turn_link_generation_nonneg_check
        CHECK (generation >= 0),
    CONSTRAINT voice_turn_link_unique_generation
        UNIQUE (guild_id, voice_channel_id, utterance_id, generation)
);

-- Reverse lookup index for `lookup_voice_turn_link_by_dispatch_id_pg`.
-- Sparse (NULL dispatch_id rows are excluded) and inexpensive.
CREATE INDEX IF NOT EXISTS idx_voice_turn_link_dispatch_id
    ON voice_turn_link (dispatch_id)
    WHERE dispatch_id IS NOT NULL;

-- Reverse lookup index for `lookup_voice_turn_link_by_announce_message_id_pg`.
CREATE INDEX IF NOT EXISTS idx_voice_turn_link_announce_message_id
    ON voice_turn_link (announce_message_id)
    WHERE announce_message_id IS NOT NULL;

-- GC sweep index — `gc_terminal_voice_turn_links_pg` reads
-- "rows whose status='terminal' AND updated_at < cutoff". Without this
-- index the cleanup pass would scan the full table at every tick.
CREATE INDEX IF NOT EXISTS idx_voice_turn_link_status_updated_at
    ON voice_turn_link (status, updated_at);
