-- #4349: record which bot provider forwarded an intake row.
--
-- Worker claim previously resolved the provider by joining `agents` and
-- reading `agents.provider`, a single column per agent. Agents that own
-- both a `discord_channel_cc` (claude) and a `discord_channel_cdx`
-- (codex) therefore claimed rows with whichever provider happened to be
-- stored on the agent — running the turn on the wrong bot's token,
-- SharedData, and mailboxes. Storing the forwarding bot's provider on
-- the row makes claim eligibility exact.

ALTER TABLE intake_outbox
  ADD COLUMN IF NOT EXISTS provider TEXT NOT NULL DEFAULT '';

-- Backfill closed rows from the agent they belonged to. Only rows in a
-- terminal status are backfilled this way: for them the (possibly wrong)
-- agent provider is simply what already ran, so it is the honest record.
-- Open rows are left at '' deliberately — see the guard below.
UPDATE intake_outbox io
SET provider = a.provider
FROM agents a
WHERE a.id = io.agent_id
  AND io.provider = ''
  AND io.status IN ('done', 'failed_pre_accept', 'failed_post_accept');

-- Any row still open at deploy time predates the provider column, so its
-- true forwarding provider is unknowable. Fail them closed rather than
-- let a worker claim one with an empty provider (which matches nothing)
-- and strand it in `pending` forever.
UPDATE intake_outbox
SET status = 'failed_pre_accept',
    last_error = 'migration 0079: open row predates provider column (#4349)'
WHERE provider = ''
  AND status IN ('pending', 'claimed', 'accepted', 'spawned');

-- Worker poll is (target_instance_id, provider) scoped now. Replace the
-- pre-#4349 index so the claim scan stays index-only.
DROP INDEX IF EXISTS idx_intake_outbox_worker_pending;

CREATE INDEX IF NOT EXISTS idx_intake_outbox_worker_pending
    ON intake_outbox (target_instance_id, provider, status, created_at)
    WHERE status = 'pending';
