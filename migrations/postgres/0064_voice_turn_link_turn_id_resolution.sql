-- #2164 Voice A follow-up: attach turn identity and route-resolution indexes.
--
-- 0060 introduced the durable voice_turn_link store before the dispatch/turn
-- call sites were wired. Later voice C-series work needs to attach those IDs
-- after a row already exists and resolve active source<->target channels.
ALTER TABLE voice_turn_link
    ADD COLUMN IF NOT EXISTS turn_id TEXT;

-- turn_id is a globally unique opaque token: once a row claims it, no other
-- row may hold the same value. The partial index (WHERE turn_id IS NOT NULL)
-- keeps the index sparse; NULL values are never deduplicated.
--
-- Conflict semantics (Rust side, attach_voice_turn_link_ids_pg):
--   * Duplicate key on this index returns AttachOutcome::Conflict (not an
--     unhandled UniqueViolation). The Rust layer checks the constraint name
--     "voice_turn_link_turn_id_uq" and converts the DB error to Conflict.
--   * insert_voice_turn_link_pg and retarget_voice_turn_link_pg likewise
--     convert this violation to Ok(None) (dedup / rejected) rather than
--     propagating the error.
CREATE UNIQUE INDEX IF NOT EXISTS voice_turn_link_turn_id_uq
    ON voice_turn_link (turn_id)
    WHERE turn_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_voice_turn_link_active_source
    ON voice_turn_link (guild_id, voice_channel_id, updated_at DESC, id DESC)
    WHERE status = 'active';

CREATE INDEX IF NOT EXISTS idx_voice_turn_link_active_target
    ON voice_turn_link (guild_id, background_channel_id, updated_at DESC, id DESC)
    WHERE status = 'active';
