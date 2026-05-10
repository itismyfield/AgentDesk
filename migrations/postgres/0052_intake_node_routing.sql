-- Phase 1 of intake-node-routing (docs/design/intake-node-routing.md).
-- Adds the durable schema for forwarding selected Discord intake
-- messages to a worker node instead of always running them on the
-- leader. No behaviour change in this migration: the new agents
-- column defaults to '[]' (no preference) and no code reads from
-- intake_outbox yet — Phase 2 wires that up.

-- Per-agent opt-in label list. Empty array = no preference (all
-- intake stays on the leader, current behaviour).
ALTER TABLE agents
    ADD COLUMN IF NOT EXISTS preferred_intake_node_labels JSONB
        NOT NULL DEFAULT '[]'::JSONB;

-- The forwarded-intake outbox. Modeled on dispatch_outbox but
-- distinct because the payload, ownership, and completion semantics
-- differ. State machine, transition SQL, and column rationale are
-- documented in docs/design/intake-node-routing.md §B / §B-bis.
CREATE TABLE IF NOT EXISTS intake_outbox (
    id                       BIGSERIAL PRIMARY KEY,

    -- Routing identity. Claim ownership is by target_instance_id
    -- (NOT by required_labels — round-2 P0 #2). required_labels is
    -- diagnostic / audit metadata.
    target_instance_id       TEXT NOT NULL,
    forwarded_by_instance_id TEXT NOT NULL,
    required_labels          JSONB NOT NULL DEFAULT '[]'::JSONB,

    -- Intake payload (parameters of handle_text_message).
    channel_id               TEXT NOT NULL,
    user_msg_id              TEXT NOT NULL,
    request_owner_id         TEXT NOT NULL,
    request_owner_name       TEXT,
    user_text                TEXT NOT NULL,
    reply_context            TEXT,
    has_reply_boundary       BOOLEAN NOT NULL DEFAULT FALSE,
    dm_hint                  BOOLEAN,
    turn_kind                TEXT NOT NULL,
    merge_consecutive        BOOLEAN NOT NULL DEFAULT FALSE,
    reply_to_user_message    BOOLEAN NOT NULL DEFAULT FALSE,
    defer_watcher_resume     BOOLEAN NOT NULL DEFAULT FALSE,
    wait_for_completion      BOOLEAN NOT NULL DEFAULT FALSE,
    agent_id                 TEXT NOT NULL,

    -- Five-state machine (round-2 P0 #2 split):
    --   pending -> claimed -> accepted -> spawned -> done
    --                                           \-> failed_post_accept
    --        \-> claimed -> failed_pre_accept (retryable via tx 10)
    -- Pre-accept failures retry by spawning a fresh row with
    -- attempt_no = family_max + 1; post-accept failures are
    -- terminal until an operator intervenes via CLI / transition 12.
    status                   TEXT NOT NULL DEFAULT 'pending',
    claim_owner              TEXT,
    claimed_at               TIMESTAMPTZ,
    accepted_at              TIMESTAMPTZ,
    spawned_at               TIMESTAMPTZ,
    completed_at             TIMESTAMPTZ,
    last_error               TEXT,
    retry_count              INTEGER NOT NULL DEFAULT 0,

    -- Per-message attempt history (round-3 P0 #1). Each retry from
    -- a terminal failed_pre_accept (or operator-confirmed
    -- retry-as-new from a post-accept terminal) creates a new row
    -- with attempt_no = MAX(family) + 1, parent_outbox_id pointing
    -- at the source. ON DELETE SET NULL so retention can prune
    -- ancestors without cascading; documented limitation.
    attempt_no               INTEGER NOT NULL DEFAULT 1,
    parent_outbox_id         BIGINT REFERENCES intake_outbox(id) ON DELETE SET NULL,

    created_at               TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Round-5 P1 #1: the constraint name discriminates this
    -- 23505 violation from the partial-index 23505 below. Rust
    -- handler matches on constraint/index name to decide whether
    -- to recompute family_max + 1 (retry) or refuse (open route
    -- exists).
    CONSTRAINT intake_outbox_unique_message_attempt
        UNIQUE (channel_id, user_msg_id, attempt_no),
    CONSTRAINT intake_outbox_attempt_no_positive
        CHECK (attempt_no >= 1),
    CONSTRAINT intake_outbox_status_check CHECK (status IN (
        'pending',
        'claimed',
        'accepted',
        'spawned',
        'done',
        'failed_pre_accept',
        'failed_post_accept'
    ))
);

-- Round-2 P0 #1: durable per-channel open-route invariant. At most
-- ONE row per channel exists in any "open" status. Concurrent
-- inserts that would create a second open row fail with
-- unique-violation; the loser re-evaluates against the existing
-- row's target. This index name is the discriminator the Rust
-- handler uses for "another open route exists" vs the 3-tuple
-- attempt-uniqueness violation above.
CREATE UNIQUE INDEX IF NOT EXISTS intake_outbox_one_open_route_per_channel
    ON intake_outbox (channel_id)
    WHERE status IN ('pending', 'claimed', 'accepted', 'spawned');

-- Worker poll: only own target. Pending rows ordered FIFO.
CREATE INDEX IF NOT EXISTS idx_intake_outbox_worker_pending
    ON intake_outbox (target_instance_id, status, created_at)
    WHERE status = 'pending';

-- Leader sweep: stale claims that never reached `accepted` (worker
-- died during cwd validation, transient failure between claim and
-- accept). Sweep promotes back to pending or failed_pre_accept.
CREATE INDEX IF NOT EXISTS idx_intake_outbox_pre_accept_sweep
    ON intake_outbox (status, claimed_at)
    WHERE status = 'claimed';

-- Leader sweep: failed_pre_accept rows with retry budget remaining
-- (transition 10 INSERTs a fresh attempt linked via parent_outbox_id).
CREATE INDEX IF NOT EXISTS idx_intake_outbox_failed_pre_accept_sweep
    ON intake_outbox (status, retry_count, updated_at)
    WHERE status = 'failed_pre_accept';

-- Round-3 P1 #3: fast SLA detector. `accepted` should usually
-- last seconds; rows past accepted_unspawned_sla_secs without
-- spawned_at fire a fast operator alert. Auto-retry forbidden
-- post-accept; the alert IS the recovery signal.
CREATE INDEX IF NOT EXISTS idx_intake_outbox_accepted_unspawned_sla
    ON intake_outbox (status, accepted_at)
    WHERE status = 'accepted';

-- Audit chain lookup: walk parent_outbox_id back to attempt_no = 1.
CREATE INDEX IF NOT EXISTS idx_intake_outbox_parent
    ON intake_outbox (parent_outbox_id)
    WHERE parent_outbox_id IS NOT NULL;

-- BEFORE-UPDATE trigger keeps `updated_at` fresh on every state
-- transition without callers having to remember it.
CREATE OR REPLACE FUNCTION intake_outbox_touch_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_intake_outbox_touch_updated_at ON intake_outbox;
CREATE TRIGGER trg_intake_outbox_touch_updated_at
    BEFORE UPDATE ON intake_outbox
    FOR EACH ROW EXECUTE FUNCTION intake_outbox_touch_updated_at();
