-- #2392 — Reverse the 3-phase persist-before-publish migration.
--
-- Restoring the original PRIMARY KEY (message_id) requires all rows to
-- carry a non-null message_id. Pending rows (message_id IS NULL) must be
-- deleted before the PK is reinstated.

DELETE FROM voice_background_handoff_meta WHERE message_id IS NULL;

ALTER TABLE voice_background_handoff_meta
    DROP CONSTRAINT IF EXISTS voice_background_handoff_meta_pkey;
DROP INDEX IF EXISTS voice_background_handoff_meta_message_id_unique;
DROP INDEX IF EXISTS voice_background_handoff_meta_correlation_id_unique;

ALTER TABLE voice_background_handoff_meta
    DROP COLUMN IF EXISTS correlation_id;
ALTER TABLE voice_background_handoff_meta
    DROP COLUMN IF EXISTS id;

ALTER TABLE voice_background_handoff_meta
    ALTER COLUMN message_id SET NOT NULL;

ALTER TABLE voice_background_handoff_meta
    ADD CONSTRAINT voice_background_handoff_meta_pkey PRIMARY KEY (message_id);
