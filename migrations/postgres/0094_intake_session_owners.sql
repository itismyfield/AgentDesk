-- #4538 PR-A — durable intake placement-owner registry (dormant schema).
--
-- Establishes the generation-fenced ownership authority that later slices
-- (#4538 PR-B/PR-C, #4548 handoff) wire into the leader intake path. This
-- migration ships schema ONLY: no production code reads or writes these
-- columns yet. The owner-CAS helpers in
-- `src/services/cluster/intake_router_hook/owner_record.rs` exercise them
-- from tests but are not called by any live route (reader flip and admission
-- wiring land in PR-C).
--
-- Design: docs/design intake-node-routing owner CAS (#4538 v3.1) §3.2.
-- Modeled 1:1 on the `voice_turn_link` history-row + monotonic generation
-- pattern (migration 0060): one immutable row per generation, at most one
-- 'active' row per identity, superseded/released rows retained as history.
--
-- Identity is `(provider, raw_channel_id)`. Both are stored normalized
-- (provider = lower(btrim()), raw_channel_id = btrim()) and the app-side
-- advisory lock key is derived from the same normalized values, so DB WHERE
-- identity and the serialization lock always agree (§3.10).
--
-- NOTE (§3.9): the `intake_outbox_open_requires_owner` CHECK and the
-- open-route unique re-alignment are ACTIVATION-phase (PR-C) steps and are
-- deliberately NOT part of this migration. 0094 is an irreversible
-- binary-floor boundary (see docs/agent-maintenance/multinode-transition.md).

CREATE TABLE IF NOT EXISTS intake_session_owners (
    id BIGSERIAL PRIMARY KEY,
    provider          TEXT NOT NULL,   -- lower(btrim()) normalized on write (§3.10)
    raw_channel_id    TEXT NOT NULL,   -- btrim() normalized on write (§3.10)
    owner_instance_id TEXT NOT NULL,
    generation        BIGINT NOT NULL,
    status            TEXT NOT NULL DEFAULT 'active',   -- active|superseded|released
    adopted_from_session BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT iso_provider_norm  CHECK (provider = lower(btrim(provider)) AND provider <> ''),
    CONSTRAINT iso_channel_norm   CHECK (raw_channel_id = btrim(raw_channel_id) AND raw_channel_id <> ''),
    CONSTRAINT iso_owner_nonempty CHECK (btrim(owner_instance_id) <> ''),
    CONSTRAINT iso_status_check   CHECK (status IN ('active','superseded','released')),
    CONSTRAINT iso_generation_nonneg CHECK (generation >= 0),
    CONSTRAINT iso_unique_generation UNIQUE (provider, raw_channel_id, generation)
);

-- At most one 'active' owner per identity. This is the schema-level backstop
-- that acquire/transfer rely on: two concurrent writers that each pass their
-- application check under READ COMMITTED cannot both leave an 'active' row —
-- the second committer fails the partial unique index and is forced to
-- re-evaluate. The app-side pg_advisory_xact_lock serializes the common case;
-- this index is the durable fence.
CREATE UNIQUE INDEX IF NOT EXISTS iso_unique_active
    ON intake_session_owners (provider, raw_channel_id) WHERE status='active';

-- Watermark lookup: acquire/transfer read the latest generation for an
-- identity (ORDER BY generation DESC LIMIT 1).
CREATE INDEX IF NOT EXISTS iso_watermark
    ON intake_session_owners (provider, raw_channel_id, generation DESC);

-- intake_outbox owner-stamp columns. NULL marks a legacy row (or a row
-- written by an older producer); the FENCE reads a NULL generation as
-- fail-closed (§3.7). owner_generation/idempotency_key are NULLABLE by design
-- (0093 preserve_on_cancel nullable-legacy pattern). The
-- open-status-requires-owner CHECK is added at activation (PR-C, §3.9).
ALTER TABLE intake_outbox ADD COLUMN IF NOT EXISTS owner_generation  BIGINT;   -- NULL=legacy
ALTER TABLE intake_outbox ADD COLUMN IF NOT EXISTS owner_instance_id TEXT;
ALTER TABLE intake_outbox ADD COLUMN IF NOT EXISTS admission_kind    TEXT NOT NULL DEFAULT 'forwarded';
ALTER TABLE intake_outbox ADD COLUMN IF NOT EXISTS idempotency_key   TEXT;

ALTER TABLE intake_outbox ADD CONSTRAINT intake_outbox_admission_kind_check
    CHECK (admission_kind IN ('local','forwarded'));

-- Idempotency dedup: ambiguous-commit retries reuse the same
-- (provider, channel, user_msg, attempt_no) key (§3.8), so a duplicate
-- admission collides here and is resolved as an idempotent hit inside the
-- admission SAVEPOINT (§3.3.2). Sparse (legacy NULL keys excluded).
CREATE UNIQUE INDEX IF NOT EXISTS intake_outbox_idempotency_key_uq
    ON intake_outbox (idempotency_key) WHERE idempotency_key IS NOT NULL;
