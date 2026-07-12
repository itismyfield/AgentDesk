-- Scheduled messages: a reservation pool that delivers Discord messages at a
-- chosen time, either directly through message_outbox ('push') or by starting
-- a headless agent turn whose relayed reply IS the delivered message ('agent').
--
-- Design: docs/design/scheduled-messages.md
--
-- Design notes:
--   * `scheduled_messages` is the definition row; `scheduled_message_deliveries`
--     is one row per fire attempt slot (routines/routine_runs pattern).
--   * uq_smdel_fire_slot + FOR UPDATE SKIP LOCKED claims make firing
--     at-most-once per (message, fire time) across cluster nodes.
--   * push handoff is terminal: once the message_outbox row exists, retry and
--     final delivery state are owned by message_outbox_loop, never re-polled
--     here. Agent deliveries stay 'running' until transcript evidence or
--     timeout (see services/scheduled_messages.rs).

CREATE TABLE IF NOT EXISTS scheduled_messages (
    id                    TEXT PRIMARY KEY,
    content               TEXT NOT NULL,
    title                 TEXT,
    -- Discord channel id; NULL only for delivery_kind='agent' (falls back to
    -- the agent's primary channel binding at fire time).
    target_channel_id     TEXT,
    bot                   TEXT NOT NULL DEFAULT 'announce',
    -- 'push' | 'agent'
    delivery_kind         TEXT NOT NULL DEFAULT 'push',
    agent_id              TEXT REFERENCES agents(id),
    agent_instruction     TEXT,
    -- 'fail' | 'push_raw'
    on_agent_failure      TEXT NOT NULL DEFAULT 'fail',
    scheduled_at          TIMESTAMPTZ NOT NULL,
    -- NULL = one-shot; otherwise '@every <duration>' or 5-field cron
    -- (same grammar as routines.schedule).
    schedule              TEXT,
    timezone              TEXT NOT NULL DEFAULT 'Asia/Seoul',
    expires_at            TIMESTAMPTZ,
    -- 'scheduled' | 'firing' | 'sent' | 'failed' | 'canceled' | 'expired'
    status                TEXT NOT NULL DEFAULT 'scheduled',
    in_flight_delivery_id TEXT,
    fire_count            BIGINT NOT NULL DEFAULT 0,
    last_fired_at         TIMESTAMPTZ,
    last_error            TEXT,
    source                TEXT NOT NULL DEFAULT 'api',
    created_by            TEXT,
    dedupe_key            TEXT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT chk_smsg_delivery_kind CHECK (delivery_kind IN ('push', 'agent')),
    CONSTRAINT chk_smsg_on_agent_failure CHECK (on_agent_failure IN ('fail', 'push_raw')),
    CONSTRAINT chk_smsg_status CHECK (status IN
        ('scheduled', 'firing', 'sent', 'failed', 'canceled', 'expired')),
    CONSTRAINT chk_smsg_agent_required CHECK
        (delivery_kind <> 'agent' OR agent_id IS NOT NULL),
    CONSTRAINT chk_smsg_push_target_required CHECK
        (delivery_kind <> 'push' OR target_channel_id IS NOT NULL)
);

-- Due-scan partial index (routines idx_routines_due_scan pattern).
CREATE INDEX IF NOT EXISTS idx_scheduled_messages_due_scan
    ON scheduled_messages(scheduled_at)
    WHERE status = 'scheduled';

CREATE INDEX IF NOT EXISTS idx_scheduled_messages_agent
    ON scheduled_messages(agent_id, status)
    WHERE agent_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_scheduled_messages_channel
    ON scheduled_messages(target_channel_id)
    WHERE target_channel_id IS NOT NULL;

-- Creation-time idempotency: dedupe_key unique among live definitions only.
CREATE UNIQUE INDEX IF NOT EXISTS uq_scheduled_messages_active_dedupe
    ON scheduled_messages(dedupe_key)
    WHERE dedupe_key IS NOT NULL
      AND status IN ('scheduled', 'firing');

CREATE TABLE IF NOT EXISTS scheduled_message_deliveries (
    id                   TEXT PRIMARY KEY,
    scheduled_message_id TEXT NOT NULL REFERENCES scheduled_messages(id),
    -- The fire slot this delivery serves; dedupe axis for at-most-once firing.
    fire_scheduled_at    TIMESTAMPTZ NOT NULL,
    delivery_kind        TEXT NOT NULL,
    -- 'running' | 'sent' | 'failed' | 'interrupted'
    status               TEXT NOT NULL DEFAULT 'running',
    claim_owner          TEXT,
    -- Per-attempt fencing token. Replaced on every re-arm so a worker whose
    -- lease expired cannot finish or rewind the replacement attempt.
    claim_token          TEXT NOT NULL,
    lease_expires_at     TIMESTAMPTZ,
    outbox_id            BIGINT,
    turn_id              TEXT,
    fallback_outbox_id   BIGINT,
    retry_count          INTEGER NOT NULL DEFAULT 0, -- agentdesk-audit: allow-int4 (bounded by the claim-time MAX_FIRE_RETRIES re-arm cap; small retry counter, not unbounded growth)
    next_attempt_at      TIMESTAMPTZ,
    error                TEXT,
    started_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at          TIMESTAMPTZ,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT chk_smdel_status CHECK (status IN
        ('running', 'sent', 'failed', 'interrupted')),
    CONSTRAINT uq_smdel_fire_slot UNIQUE (scheduled_message_id, fire_scheduled_at)
);

CREATE INDEX IF NOT EXISTS idx_smdel_parent
    ON scheduled_message_deliveries(scheduled_message_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_smdel_running_lease
    ON scheduled_message_deliveries(lease_expires_at)
    WHERE status = 'running';

CREATE INDEX IF NOT EXISTS idx_smdel_turn_id
    ON scheduled_message_deliveries(turn_id)
    WHERE turn_id IS NOT NULL;
