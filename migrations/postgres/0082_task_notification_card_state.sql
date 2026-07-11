-- #4055: durable single authority for task-notification completion cards.
--
-- A logical provider event owns exactly one row. The stable Discord nonce lets
-- a retry reconcile an ambiguous create response (Discord accepted the POST but
-- the client did not observe the response) without creating a second card.
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

CREATE INDEX IF NOT EXISTS idx_task_notification_card_state_lease
    ON task_notification_card_state (lease_expires_at)
    WHERE lease_owner IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_task_notification_card_state_retention
    ON task_notification_card_state (updated_at);
