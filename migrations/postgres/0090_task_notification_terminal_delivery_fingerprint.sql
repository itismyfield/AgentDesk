-- #4295: exact terminal task-notification replay guard.
--
-- Transcript compaction can move a previously delivered user entry to a new
-- byte offset. The provider entry id survives that rewrite, so retain its
-- fingerprint beside the delivered Discord message id on the semantic card
-- row. The fingerprint supplements (and never replaces) the semantic event key
-- shared by prompt observation and terminal response promotion.
ALTER TABLE task_notification_card_state
    ADD COLUMN terminal_delivery_fingerprint VARCHAR(64)
        CHECK (
            terminal_delivery_fingerprint IS NULL
            OR char_length(terminal_delivery_fingerprint) = 64
        );

CREATE UNIQUE INDEX idx_task_notification_terminal_delivery_fingerprint
    ON task_notification_card_state
        (channel_id, provider, terminal_delivery_fingerprint)
    WHERE terminal_delivery_fingerprint IS NOT NULL;

-- Card rows retain the current semantic completion. This ledger preserves
-- delivered identities when a legitimate later completion for the same task
-- advances that semantic row to a new physical card.
CREATE TABLE task_notification_terminal_delivery (
    id BIGSERIAL PRIMARY KEY,
    channel_id BIGINT NOT NULL CHECK (channel_id > 0),
    provider TEXT NOT NULL CHECK (btrim(provider) <> ''),
    session_key TEXT NOT NULL CHECK (btrim(session_key) <> ''),
    event_key TEXT NOT NULL CHECK (btrim(event_key) <> ''),
    terminal_delivery_fingerprint VARCHAR(64)
        CHECK (
            terminal_delivery_fingerprint IS NULL
            OR char_length(terminal_delivery_fingerprint) = 64
        ),
    discord_message_id BIGINT NOT NULL CHECK (discord_message_id > 0),
    bot_key TEXT NOT NULL CHECK (btrim(bot_key) <> ''),
    content_hash VARCHAR(64) NOT NULL CHECK (char_length(content_hash) = 64),
    delivered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (channel_id, provider, session_key, event_key, discord_message_id)
);

CREATE UNIQUE INDEX idx_task_notification_terminal_delivery_source
    ON task_notification_terminal_delivery
        (channel_id, provider, terminal_delivery_fingerprint)
    WHERE terminal_delivery_fingerprint IS NOT NULL;

CREATE INDEX idx_task_notification_terminal_delivery_semantic
    ON task_notification_terminal_delivery
        (channel_id, provider, event_key, content_hash)
    WHERE terminal_delivery_fingerprint IS NULL;

CREATE INDEX idx_task_notification_terminal_delivery_retention
    ON task_notification_terminal_delivery (delivered_at);
