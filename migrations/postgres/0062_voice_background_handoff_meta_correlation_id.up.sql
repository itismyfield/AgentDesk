-- #2392 — Persist-before-publish handoff ordering.
--
-- The pre-#2392 flow was:
--   1. driver.start() posts the announce-bot message → returns message_id
--   2. persist_handoff_durable inserts the PG row keyed by message_id
--
-- A fast downstream turn (e.g. cached / no LLM call) could complete and
-- consult `voice_background_completion_target` while the marker store was
-- still empty, silently dropping the spoken-summary routing. The #2351 v6
-- Option C local fallback did not cover this because both the durable PG
-- row and the in-memory marker were equally absent during the window.
--
-- The fix splits dispatch into three phases:
--   1. reserve_handoff(correlation_id, meta) — pre-publish PG INSERT with
--      message_id = NULL. correlation_id is computable from
--      (guild_id, voice_channel_id, utterance_id, generation) BEFORE
--      publish, so the durable side store has the row before any caller
--      can observe MESSAGE_CREATE for the published message.
--   2. driver.start() — Discord publish, returns message_id.
--   3. bind_handoff_message_id(correlation_id, message_id) — atomic
--      UPDATE to promote the pending row into a message_id-keyed marker.
--
-- This migration adds the `correlation_id` column, drops the previously
-- mandatory NOT NULL on `message_id` so a pending row can exist before
-- publish, and adds a partial unique index on `correlation_id` so a
-- double reservation is rejected by the schema rather than silently
-- overwritten. Legacy rows with `correlation_id IS NULL` are tolerated
-- during the deploy window and GC'd by TTL.

ALTER TABLE voice_background_handoff_meta
    ADD COLUMN IF NOT EXISTS correlation_id TEXT;

-- The pre-#2392 PRIMARY KEY (message_id) prevented `message_id IS NULL`.
-- Replace the PK with a partial unique index on the non-NULL prefix so
-- pending rows (NULL message_id) can coexist before bind, but two
-- committed rows with the same message_id are still rejected by the
-- schema.
ALTER TABLE voice_background_handoff_meta
    DROP CONSTRAINT IF EXISTS voice_background_handoff_meta_pkey;

-- Drop the implicit NOT NULL on `message_id` that came with the
-- PRIMARY KEY. Pending reservations carry NULL until bind.
ALTER TABLE voice_background_handoff_meta
    ALTER COLUMN message_id DROP NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS voice_background_handoff_meta_message_id_unique
    ON voice_background_handoff_meta (message_id)
    WHERE message_id IS NOT NULL;

-- Two concurrent reservations against the same correlation_id (gen, voice
-- channel, utterance) must be rejected — silent overwrite was Codex
-- HIGH-3 against PR #2446.
CREATE UNIQUE INDEX IF NOT EXISTS voice_background_handoff_meta_correlation_id_unique
    ON voice_background_handoff_meta (correlation_id)
    WHERE correlation_id IS NOT NULL;

-- A new synthetic row id is needed because (message_id, NULL) cannot
-- serve as PK. Use a SERIAL bigint to keep ordering well-defined for GC
-- sweeps without affecting any reader.
ALTER TABLE voice_background_handoff_meta
    ADD COLUMN IF NOT EXISTS id BIGSERIAL;

-- Enforce a single non-null PK for new rows; legacy migrated rows pick up
-- the BIGSERIAL value populated during the column add.
ALTER TABLE voice_background_handoff_meta
    ADD CONSTRAINT voice_background_handoff_meta_pkey PRIMARY KEY (id);
