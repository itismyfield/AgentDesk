-- #2362 / #2164 Voice A — VoiceTurnLink durable store rollback.
DROP INDEX IF EXISTS voice_turn_link_unique_active;
DROP INDEX IF EXISTS idx_voice_turn_link_status_updated_at;
DROP INDEX IF EXISTS idx_voice_turn_link_announce_message_id;
DROP INDEX IF EXISTS idx_voice_turn_link_dispatch_id;
DROP TABLE IF EXISTS voice_turn_link;
