-- #2164 Voice A follow-up: attach turn identity and route-resolution indexes.
--
-- 0060 introduced the durable voice_turn_link store before the dispatch/turn
-- call sites were wired. Later voice C-series work needs to attach those IDs
-- after a row already exists and resolve active source<->target channels.
ALTER TABLE voice_turn_link
    ADD COLUMN IF NOT EXISTS turn_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS voice_turn_link_turn_id_uq
    ON voice_turn_link (turn_id)
    WHERE turn_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_voice_turn_link_active_source
    ON voice_turn_link (guild_id, voice_channel_id, updated_at DESC, id DESC)
    WHERE status = 'active';

CREATE INDEX IF NOT EXISTS idx_voice_turn_link_active_target
    ON voice_turn_link (guild_id, background_channel_id, updated_at DESC, id DESC)
    WHERE status = 'active';
