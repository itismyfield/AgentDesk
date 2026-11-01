-- Voice is removed; nothing reads or writes these tables any more. Their indexes drop with them.
DROP TABLE IF EXISTS voice_turn_link;
DROP TABLE IF EXISTS voice_background_handoff_meta;
DROP TABLE IF EXISTS voice_transcript_announcement_meta;
-- Dropping a table removes its triggers but not their functions.
DROP FUNCTION IF EXISTS voice_background_handoff_meta_touch_updated_at();
DROP FUNCTION IF EXISTS voice_transcript_announcement_meta_touch_updated_at();
