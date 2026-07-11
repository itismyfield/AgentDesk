-- #4055: durable single authority for task-notification completion cards.
--
-- A logical provider event owns exactly one row. Within Discord's bounded nonce
-- replay window, the stable nonce lets a retry reconcile an ambiguous create
-- response; the row/message id remains the long-lived authority.
CREATE TABLE IF NOT EXISTS task_notification_card_state (
    id BIGSERIAL PRIMARY KEY,
    channel_id BIGINT NOT NULL CHECK (channel_id > 0),
    provider TEXT NOT NULL CHECK (btrim(provider) <> ''),
    session_key TEXT NOT NULL CHECK (btrim(session_key) <> ''),
    event_key TEXT NOT NULL CHECK (btrim(event_key) <> ''),
    surface_owner TEXT NOT NULL
        CHECK (surface_owner IN ('footer_only', 'card')),
    delivery_state TEXT NOT NULL
        CHECK (delivery_state IN ('footer_only', 'posting', 'card_posted')),
    bot_key TEXT NOT NULL DEFAULT '',
    discord_nonce VARCHAR(25) NOT NULL
        CHECK (char_length(discord_nonce) BETWEEN 1 AND 25),
    discord_message_id BIGINT CHECK (discord_message_id > 0),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1),
    update_count BIGINT NOT NULL DEFAULT 1 CHECK (update_count >= 1),
    rendered_content TEXT NOT NULL DEFAULT '',
    content_hash VARCHAR(64) NOT NULL CHECK (char_length(content_hash) = 64),
    lease_owner TEXT,
    lease_expires_at TIMESTAMPTZ,
    -- First persisted crossing of the Discord POST network boundary. NULL
    -- means the durable intent is still safe to post; a non-NULL expired
    -- attempt must stay within the bounded nonce window or reconcile history.
    post_started_at TIMESTAMPTZ,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (channel_id, provider, session_key, event_key),
    CHECK (delivery_state <> 'card_posted' OR discord_message_id IS NOT NULL),
    CHECK ((surface_owner = 'footer_only') = (delivery_state = 'footer_only')),
    CHECK (delivery_state = 'footer_only' OR btrim(bot_key) <> ''),
    CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL))
);

-- A card is a semantic-event surface, while responses are per terminal turn.
-- Keeping response claims 1:N preserves delivered tombstones for sequential
-- turns and lets an expired sink claim be taken over without rewriting history.
CREATE TABLE IF NOT EXISTS task_notification_response_delivery (
    id BIGSERIAL PRIMARY KEY,
    channel_id BIGINT NOT NULL CHECK (channel_id > 0),
    provider TEXT NOT NULL CHECK (btrim(provider) <> ''),
    session_key TEXT NOT NULL CHECK (btrim(session_key) <> ''),
    event_key TEXT NOT NULL CHECK (btrim(event_key) <> ''),
    response_turn_key VARCHAR(64) NOT NULL
        CHECK (char_length(response_turn_key) = 64),
    -- Actor-independent recovery alias. The live sink may know the frame key
    -- while a restarted watcher only knows terminal offset/body identity.
    recovery_turn_key VARCHAR(64)
        CHECK (recovery_turn_key IS NULL OR char_length(recovery_turn_key) = 64),
    -- Monotonic transcript boundaries identify one logical provider turn even
    -- when sink/watcher response bytes produce divergent fallback keys.
    turn_start_offset BIGINT CHECK (turn_start_offset IS NULL OR turn_start_offset >= 0),
    turn_end_offset BIGINT CHECK (turn_end_offset IS NULL OR turn_end_offset >= 0),
    referenced_card_message_id BIGINT NOT NULL
        CHECK (referenced_card_message_id > 0),
    response_generation INTEGER NOT NULL DEFAULT 1
        CHECK (response_generation >= 1),
    delivery_state TEXT NOT NULL
        CHECK (delivery_state IN ('claimed', 'sent', 'delivered')),
    owner_kind TEXT CHECK (owner_kind IN ('sink', 'watcher')),
    owner_token TEXT,
    lease_expires_at TIMESTAMPTZ,
    sent_at TIMESTAMPTZ,
    delivered_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (channel_id, provider, session_key, response_turn_key),
    CHECK (turn_start_offset IS NULL OR turn_end_offset IS NULL
        OR turn_end_offset >= turn_start_offset),
    CHECK (
        (delivery_state = 'claimed'
            AND owner_kind IS NOT NULL
            AND owner_token IS NOT NULL
            AND lease_expires_at IS NOT NULL
            AND sent_at IS NULL
            AND delivered_at IS NULL)
        OR
        (delivery_state = 'sent'
            AND owner_kind IS NOT NULL
            AND owner_token IS NOT NULL
            AND lease_expires_at IS NOT NULL
            AND sent_at IS NOT NULL
            AND delivered_at IS NULL)
        OR
        (delivery_state = 'delivered'
            AND owner_kind IS NULL
            AND owner_token IS NULL
            AND lease_expires_at IS NULL
            AND sent_at IS NOT NULL
            AND delivered_at IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_task_notification_response_claim_lease
    ON task_notification_response_delivery (lease_expires_at)
    WHERE delivery_state IN ('claimed', 'sent');

CREATE UNIQUE INDEX IF NOT EXISTS idx_task_notification_response_recovery_key
    ON task_notification_response_delivery
        (channel_id, provider, session_key, recovery_turn_key)
    WHERE recovery_turn_key IS NOT NULL;

-- Divergent sink/watcher fallback bytes must still converge while one logical
-- response is active. Sequential delivered responses remain permitted.
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_notification_response_active_event
    ON task_notification_response_delivery
        (channel_id, provider, session_key, event_key)
    WHERE delivery_state IN ('claimed', 'sent');

CREATE INDEX IF NOT EXISTS idx_task_notification_response_retention
    ON task_notification_response_delivery (updated_at);

-- A durable pre-POST journal closes the unbounded gap between Discord accept
-- and the response-row sent CAS. Generations prevent a nonce that belonged to
-- an old/deleted required-reference card from being reused after repair.
CREATE TABLE IF NOT EXISTS task_notification_response_chunk (
    response_delivery_id BIGINT NOT NULL
        REFERENCES task_notification_response_delivery(id) ON DELETE CASCADE,
    response_generation INTEGER NOT NULL CHECK (response_generation >= 1),
    chunk_index INTEGER NOT NULL CHECK (chunk_index >= 0),
    chunk_count BIGINT NOT NULL CHECK (chunk_count > 0 AND chunk_index < chunk_count),
    content_hash VARCHAR(64) NOT NULL CHECK (char_length(content_hash) = 64),
    discord_nonce VARCHAR(25) NOT NULL CHECK (char_length(discord_nonce) BETWEEN 1 AND 25),
    bot_user_id BIGINT NOT NULL CHECK (bot_user_id > 0),
    referenced_message_id BIGINT CHECK (referenced_message_id > 0),
    -- `prepared` is durable intent that has not crossed the network boundary;
    -- `posting` means an HTTP attempt may have reached Discord and therefore
    -- requires nonce/history reconciliation after a crash.
    delivery_state TEXT NOT NULL
        CHECK (delivery_state IN ('prepared', 'posting', 'confirmed')),
    discord_message_id BIGINT CHECK (discord_message_id > 0),
    attempt_started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    post_started_at TIMESTAMPTZ,
    confirmed_at TIMESTAMPTZ,
    last_reconcile_error TEXT,
    next_reconcile_at TIMESTAMPTZ,
    alert_count BIGINT NOT NULL DEFAULT 0 CHECK (alert_count >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (response_delivery_id, response_generation, chunk_index),
    CHECK (
        (delivery_state = 'prepared'
            AND post_started_at IS NULL
            AND discord_message_id IS NULL
            AND confirmed_at IS NULL)
        OR
        (delivery_state = 'posting'
            AND post_started_at IS NOT NULL
            AND discord_message_id IS NULL
            AND confirmed_at IS NULL)
        OR
        (delivery_state = 'confirmed'
            AND post_started_at IS NOT NULL
            AND discord_message_id IS NOT NULL
            AND confirmed_at IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_task_notification_response_chunk_reconcile
    ON task_notification_response_chunk (next_reconcile_at)
    WHERE delivery_state = 'posting';

CREATE INDEX IF NOT EXISTS idx_task_notification_card_state_lease
    ON task_notification_card_state (lease_expires_at)
    WHERE lease_owner IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_task_notification_card_state_retention
    ON task_notification_card_state (updated_at);
