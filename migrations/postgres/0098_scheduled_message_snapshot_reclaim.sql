-- #4723: real, FK-safe reclaim for scheduled-message context snapshots
-- (follow-up to #4658 F3).
--
-- Problem recap: `fk_smsg_context_snapshot` (scheduled_messages.context_snapshot_id
-- → scheduled_message_context_snapshots.id) pins a snapshot for as long as ANY
-- definition references it. Nothing deletes a `scheduled_messages` row (cancel
-- keeps the row), so a terminal snapshot definition kept its snapshot referenced
-- forever and the 30-day reclaim only ever fired for genuinely unreferenced rows.
-- Time-based deletion of the snapshot alone is impossible: the terminal
-- definition still references it, so the DELETE would violate the FK.
--
-- Chosen FK-safe reclaim: for a snapshot whose EVERY referencing definition is
-- terminal AND aged past the retention window, null the FK pointer on those
-- terminal definitions, then delete the now-unreferenced snapshot. Provenance is
-- preserved — `context_strategy` stays 'snapshot' and a new
-- `context_snapshot_reclaimed_at` timestamp records that the frozen context was
-- reclaimed by retention (rather than the definition having been 'fresh'). This
-- resolves #4658-F3 Option 2's objection (losing the "was snapshot strategy"
-- fact): the fact is retained; only the bounded ≤32KB rendered_context row is
-- freed. Active/pending definitions (status 'scheduled'/'firing') are never
-- terminal, so AC-9 (an active recurring reservation's snapshot is never pruned)
-- stays structural.

ALTER TABLE scheduled_messages
    ADD COLUMN IF NOT EXISTS context_snapshot_reclaimed_at TIMESTAMPTZ;

-- Supports the retention reference scan (correlated lookup by snapshot id) for
-- both the pre-existing unreferenced predicate and the new all-terminal reclaim.
CREATE INDEX IF NOT EXISTS idx_smsg_context_snapshot_id
    ON scheduled_messages (context_snapshot_id)
    WHERE context_snapshot_id IS NOT NULL;

-- Relax chk_smsg_snapshot_required to admit the reclaimed state. Recreated
-- (drop + add) so re-runs converge on the new definition; the DROP ... IF EXISTS
-- + guarded ADD keep the migration idempotent.
ALTER TABLE scheduled_messages
    DROP CONSTRAINT IF EXISTS chk_smsg_snapshot_required;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'chk_smsg_snapshot_required'
    ) THEN
        ALTER TABLE scheduled_messages
            ADD CONSTRAINT chk_smsg_snapshot_required
            CHECK (
                -- snapshot strategy, snapshot still captured (normal life)
                (context_strategy = 'snapshot'
                    AND context_snapshot_id IS NOT NULL
                    AND context_snapshot_reclaimed_at IS NULL)
                -- snapshot strategy, snapshot reclaimed by retention (provenance kept)
             OR (context_strategy = 'snapshot'
                    AND context_snapshot_id IS NULL
                    AND context_snapshot_reclaimed_at IS NOT NULL
                    AND status IN ('sent', 'failed', 'canceled', 'expired'))
                -- fresh strategy never carries a snapshot or a reclaim marker
             OR (context_strategy = 'fresh'
                    AND context_snapshot_id IS NULL
                    AND context_snapshot_reclaimed_at IS NULL)
            );
    END IF;
END $$;
