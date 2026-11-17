-- Whether a stored turn takes part in its channel's conversation context.
--
-- session_transcripts.channel_context  FALSE keeps a turn as completion evidence under its
--                                      channel_id but out of that channel's recent-context,
--                                      snapshot and snapshot-frontier reads. Existing rows join.

ALTER TABLE IF EXISTS session_transcripts
    ADD COLUMN IF NOT EXISTS channel_context BOOLEAN NOT NULL DEFAULT TRUE;
