-- Close the ambiguity window between reserving a synthetic agent turn and
-- invoking the external provider runtime. A launch commit is an at-most-once
-- barrier: after it is durable, recovery may poll/fail closed but must never
-- start a replacement turn. `turn_started_at` remains the later runtime ack.
ALTER TABLE scheduled_message_deliveries
    ADD COLUMN IF NOT EXISTS turn_intent_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS launch_committed_at TIMESTAMPTZ;

-- Rows created by the public 0084 lineage can already have a durable turn id
-- without either phase timestamp. Their launch state is unknowable after an
-- upgrade, so classify them conservatively as committed/started rather than
-- risking a duplicate replacement turn.
--
-- `turn_intent_at` deliberately stays NULL for these rows. During a rolling
-- deploy, that also lets the new reader recognize a turn id written by an old
-- binary after this migration as legacy/ambiguous and adopt it fail-closed.
UPDATE scheduled_message_deliveries
SET launch_committed_at = COALESCE(launch_committed_at, turn_started_at, started_at),
    turn_started_at = COALESCE(turn_started_at, started_at),
    updated_at = NOW()
WHERE turn_id IS NOT NULL
  AND (launch_committed_at IS NULL OR turn_started_at IS NULL);

-- Runtime bootstrap failures are prerequisites, not delivery attempts. Keep
-- the original recurrence anchor in `scheduled_at` while this independent
-- not-before gate prevents the due worker from hot-looping overdue rows.
ALTER TABLE scheduled_messages
    ADD COLUMN IF NOT EXISTS runtime_defer_until TIMESTAMPTZ;
