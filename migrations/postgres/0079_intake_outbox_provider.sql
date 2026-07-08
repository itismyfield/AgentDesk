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

-- 1. Exact recovery. `claim_owner` is written as "{instance_id}:{provider}"
--    (runtime_bootstrap/intake.rs), and `instance_id` is restricted to
--    [A-Za-z0-9._-], so the suffix is unambiguous. Every row a worker ever
--    claimed carries it. `sweep_stale_pre_accept_claims` nulls `claim_owner`
--    when it resets `claimed -> pending`, so a surviving value always names
--    the bot that actually held the row.
UPDATE intake_outbox
SET provider = split_part(claim_owner, ':', 2)
WHERE provider = ''
  AND claim_owner IS NOT NULL
  AND split_part(claim_owner, ':', 2) IN ('claude', 'codex', 'gemini', 'qwen', 'opencode');

-- 2. Rows that died before any worker claimed them but are already terminal.
--    Nothing will ever re-read their provider, so the owning agent's provider
--    is the closest honest record.
UPDATE intake_outbox io
SET provider = a.provider
FROM agents a
WHERE a.id = io.agent_id
  AND io.provider = ''
  AND io.status IN ('done', 'failed_pre_accept', 'failed_post_accept');

-- 3. Open PRE-accept rows with no recoverable provider. An empty provider
--    matches no worker, so leaving them `pending` strands them forever.
--    `pending` and `claimed` are both pre-accept: the sweep already resets
--    `claimed -> pending`, and `mark_failed_pre_accept` is their normal
--    failure path, so a retryable terminal state is correct here.
UPDATE intake_outbox
SET status = 'failed_pre_accept',
    last_error = 'migration 0079: provider unrecoverable for pre-accept row (#4349)'
WHERE provider = ''
  AND status IN ('pending', 'claimed');

-- 4. Open POST-accept rows with no recoverable provider. `accepted` and
--    `spawned` mean the worker already validated cwd and may have spawned —
--    the turn can already have emitted to Discord. Auto-retry is forbidden
--    past `accepted` (intake_worker.rs: "a failure is post-accept and is NOT
--    auto-retried"; the operator alert IS the recovery signal), so these must
--    never be labelled pre-accept. Mark them post-accept and leave recovery to
--    `force_fail_and_retry_as_new`, whose TRANSITION_12_ALLOWED accepts exactly
--    this state.
--
--    Reaching here requires a malformed `claim_owner`, since `mark_accepted`
--    only advances a row whose `claim_owner` matches. Kept as a defensive arm.
UPDATE intake_outbox
SET status = 'failed_post_accept',
    completed_at = COALESCE(completed_at, NOW()),
    last_error = 'migration 0079: provider unrecoverable for post-accept row; operator recovery required (#4349)'
WHERE provider = ''
  AND status IN ('accepted', 'spawned');

-- Worker poll is (target_instance_id, provider) scoped now. Replace the
-- pre-#4349 index so the claim scan stays index-only.
DROP INDEX IF EXISTS idx_intake_outbox_worker_pending;

CREATE INDEX IF NOT EXISTS idx_intake_outbox_worker_pending
    ON intake_outbox (target_instance_id, provider, status, created_at)
    WHERE status = 'pending';
