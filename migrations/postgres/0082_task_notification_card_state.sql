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
    referenced_card_message_id BIGINT NOT NULL
        CHECK (referenced_card_message_id > 0),
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

CREATE INDEX IF NOT EXISTS idx_task_notification_response_retention
    ON task_notification_response_delivery (updated_at);

CREATE INDEX IF NOT EXISTS idx_task_notification_card_state_lease
    ON task_notification_card_state (lease_expires_at)
    WHERE lease_owner IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_task_notification_card_state_retention
    ON task_notification_card_state (updated_at);
