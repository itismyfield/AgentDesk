-- #4091 r4 follow-up: make raw Claude transcript growth evidence durable
-- without letting observations for one raw session id poison another.
--
-- `raw_provider_transcript_watermark_session_id` records which raw transcript
-- id owns `raw_provider_transcript_len_watermark`. Selectors must ignore the
-- length watermark when this id differs from the currently observed raw id.
-- `raw_provider_transcript_growth_proven` is sticky proof that this raw
-- transcript grew past its prior valid watermark; advancing the watermark later
-- must not destroy that proof.

ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS raw_provider_transcript_watermark_session_id TEXT;

ALTER TABLE sessions
  ADD COLUMN IF NOT EXISTS raw_provider_transcript_growth_proven BOOLEAN NOT NULL DEFAULT FALSE;

UPDATE sessions
   SET raw_provider_transcript_watermark_session_id = raw_provider_session_id
 WHERE raw_provider_session_id IS NOT NULL
   AND BTRIM(raw_provider_session_id) != ''
   AND COALESCE(raw_provider_transcript_len_watermark, 0) > 0
   AND raw_provider_transcript_watermark_session_id IS NULL;
