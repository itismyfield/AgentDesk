-- #4658: immutable conversation-context snapshots for scheduled agent turns.
--
-- A `context_strategy='snapshot'` reservation freezes the channel's live
-- conversation context at creation time into an immutable row here. The fire
-- path validates the row (digest recompute) before the launch-commit barrier
-- and injects `rendered_context` into a fresh, isolated provider session, so a
-- scheduled turn always runs against the context that existed at schedule time
-- rather than whatever is live at fire time.
--
-- `rendered_context` is stored fully at capture (not a frontier pointer) so the
-- snapshot stays reproducible even after `session_transcripts` are archived and
-- DELETEd by the 90-day retention job or a `/clear` boundary intervenes.
-- `content_digest` is the SHA-256 of the canonicalized rendered_context + meta;
-- the fire path recomputes it and fails closed on any mismatch.
--
-- Backward compatibility: every existing and new `fresh` reservation is
-- untouched — `context_strategy` defaults to 'fresh', the two new companion
-- columns stay NULL/'fail', and the fire path skips all snapshot logic.

CREATE TABLE IF NOT EXISTS scheduled_message_context_snapshots (
    id                  TEXT PRIMARY KEY,
    source_channel_id   TEXT NOT NULL,
    source_session_key  TEXT,
    -- Immutable boundary: the last session_transcripts.id observed at capture.
    -- Kept for audit/provenance; the rendered pairs are stored inline below so
    -- the snapshot never re-reads transcripts at fire time.
    transcript_frontier BIGINT NOT NULL,
    -- Frozen rendering of the frontier-bounded recent pairs (bounded per policy).
    rendered_context    TEXT NOT NULL,
    pair_count          INTEGER NOT NULL, -- agentdesk-audit: allow-int4 (bounded by SNAPSHOT_MAX_PAIRS=10 captured pairs; small fixed-ceiling counter, not unbounded growth)
    -- Execution intent (conversation/model intent only — never tool permissions,
    -- sandbox/approval, or allowlist policy, which are re-resolved at fire time).
    provider            TEXT,
    model               TEXT,
    reasoning_effort    TEXT,
    fast_mode           BOOLEAN,
    workspace_hint      TEXT,
    -- Integrity: SHA-256 over rendered_context + captured meta.
    content_digest      TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT chk_smcs_source_channel_norm
        CHECK (source_channel_id = btrim(source_channel_id) AND source_channel_id <> ''),
    CONSTRAINT chk_smcs_pair_count_nonneg CHECK (pair_count >= 0),
    CONSTRAINT chk_smcs_digest_hex
        CHECK (content_digest ~ '^[0-9a-f]{64}$')
);

CREATE INDEX IF NOT EXISTS idx_smcs_source_channel
    ON scheduled_message_context_snapshots (source_channel_id);

ALTER TABLE scheduled_messages
    ADD COLUMN IF NOT EXISTS context_strategy TEXT NOT NULL DEFAULT 'fresh',
    ADD COLUMN IF NOT EXISTS context_snapshot_id TEXT,
    ADD COLUMN IF NOT EXISTS on_context_failure TEXT NOT NULL DEFAULT 'fail';

-- Named constraints added separately so re-runs are idempotent and the intent
-- is legible in \d output.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'chk_smsg_context_strategy'
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT chk_smsg_context_strategy
            CHECK (context_strategy IN ('fresh', 'snapshot'));
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'chk_smsg_on_context_failure'
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT chk_smsg_on_context_failure
            CHECK (on_context_failure IN ('fail', 'fresh'));
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'fk_smsg_context_snapshot'
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT fk_smsg_context_snapshot
            FOREIGN KEY (context_snapshot_id)
            REFERENCES scheduled_message_context_snapshots(id);
    END IF;

    -- snapshot strategy requires a captured snapshot; fresh must not carry one.
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'chk_smsg_snapshot_required'
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT chk_smsg_snapshot_required
            CHECK (
                (context_strategy = 'snapshot' AND context_snapshot_id IS NOT NULL)
             OR (context_strategy = 'fresh' AND context_snapshot_id IS NULL)
            );
    END IF;
END $$;
