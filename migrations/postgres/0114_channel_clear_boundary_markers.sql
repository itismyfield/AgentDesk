-- #5707: durable clear-authority markers on the channel clear boundary row.
--
-- `clear_generation` advances on every boundary write inside the channel's
-- advisory lock, so two observations that are equal prove no clear serialized
-- between them without comparing any clock.
--
-- `cleared_through_id` records the `session_transcripts.id` frontier the clear
-- observed while holding that same lock, so a transcript that committed before
-- the clear took the lock stays covered even when its `created_at` is later
-- than `cleared_at`.
--
-- No backfill. Existing rows take 0 and `id > 0` holds for every transcript
-- row, so channel reads stay bit-identical until the next clear writes a real
-- frontier.
ALTER TABLE channel_session_clear_boundaries
    ADD COLUMN IF NOT EXISTS cleared_through_id BIGINT NOT NULL DEFAULT 0;

ALTER TABLE channel_session_clear_boundaries
    ADD COLUMN IF NOT EXISTS clear_generation BIGINT NOT NULL DEFAULT 0;
