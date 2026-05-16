-- #2209 rollback — drop the durable voice-announce metadata table.
--
-- Down migration is destructive: any unconsumed durable announce rows
-- are lost. The in-memory `voice::announce_meta` store still works as
-- a best-effort cache, but cross-process intake workers + dcserver
-- restart paths lose their durable recovery mechanism.
DROP TRIGGER IF EXISTS trg_voice_transcript_announce_meta_touch_updated_at
    ON voice_transcript_announce_meta;
DROP FUNCTION IF EXISTS voice_transcript_announce_meta_touch_updated_at();
DROP INDEX IF EXISTS idx_voice_transcript_announce_meta_created_at;
DROP TABLE IF EXISTS voice_transcript_announce_meta;
