-- #4091 follow-up: split-brain selector recovery needs two pieces of
-- per-session state that must survive dcserver restarts.
--
-- 1. `claude_session_id_recorded_at` anchors the short missing-transcript grace
--    window to when the cached Claude selector value was written. Heartbeats and
--    status PATCHes must not keep extending that window forever.
-- 2. `raw_provider_transcript_len_watermark` is a monotonic length watermark for
--    the raw Claude transcript selector. A later observation whose file length
--    exceeds this value is durable growth evidence even after process restart.

ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS claude_session_id_recorded_at TIMESTAMPTZ;

UPDATE sessions
   SET claude_session_id_recorded_at = COALESCE(last_heartbeat, created_at, NOW())
 WHERE claude_session_id IS NOT NULL
   AND BTRIM(claude_session_id) != ''
   AND claude_session_id_recorded_at IS NULL;

ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS raw_provider_transcript_len_watermark BIGINT NOT NULL DEFAULT 0;
