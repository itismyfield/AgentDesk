-- 0139_imsg_reply_obligations.sql [NEW]
CREATE TABLE imsg_reply_obligations (
    origin_id             TEXT PRIMARY KEY,
    source                TEXT NOT NULL DEFAULT 'imessage',
    agent_id              TEXT NOT NULL,
    channel_id            TEXT NOT NULL,
    person_id             BIGINT NOT NULL,
    request_hash          TEXT NOT NULL,

    opened_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at            TIMESTAMPTZ NOT NULL,

    admission             TEXT NOT NULL DEFAULT 'admitting',
    admission_kind        TEXT,
    admission_result      JSONB,
    delivery_confirmed    BOOLEAN,

    resolution            TEXT NOT NULL DEFAULT 'open',

    admitted_turn_id      TEXT,
    queued_message_id     TEXT,
    episode_key           TEXT,
    inject_nonce          TEXT,

    last_reply_episode    TEXT,
    last_reply_at         TIMESTAMPTZ,

    terminal_seen_at      TIMESTAMPTZ,
    terminal_first_line   TEXT,

    active_send_action    UUID,
    discord_mirror_id     TEXT,

    closed_at             TIMESTAMPTZ,
    closed_reason         TEXT,

    CHECK (source = 'imessage'),
    CHECK (person_id > 0),
    CHECK (LENGTH(origin_id) BETWEEN 1 AND 256),
    CHECK (expires_at = opened_at + INTERVAL '24 hours'),

    CHECK (admission IN (
        'admitting', 'admitted', 'refused', 'unknown'
    )),
    CHECK (admission_kind IS NULL OR admission_kind IN (
        'started', 'queued', 'injected', 'unconfirmed'
    )),
    CHECK (resolution IN (
        'open', 'replied', 'expired', 'closed'
    ))
);

CREATE TABLE imsg_reply_actions (
    id                 UUID PRIMARY KEY,
    origin_id          TEXT NOT NULL
        REFERENCES imsg_reply_obligations(origin_id)
        ON DELETE RESTRICT,

    purpose            TEXT NOT NULL,
    action_key         TEXT NOT NULL,
    status             TEXT NOT NULL DEFAULT 'claimed',

    reply_episode_key  TEXT,
    claimed_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    sent_at            TIMESTAMPTZ,
    receipt            JSONB,

    UNIQUE (origin_id, id),
    UNIQUE (origin_id, purpose, action_key),

    CHECK (purpose IN (
        'progress', 'final', 'fallback', 'expiry', 'mirror'
    )),
    CHECK (status IN (
        'claimed', 'sent', 'unknown', 'not_sent'
    )),
    CHECK ((status = 'sent') = (sent_at IS NOT NULL))
);

-- A claimed external send must belong to the same obligation.
ALTER TABLE imsg_reply_obligations
ADD CONSTRAINT imsg_active_action_origin_fk
FOREIGN KEY (origin_id, active_send_action)
REFERENCES imsg_reply_actions(origin_id, id)
DEFERRABLE INITIALLY DEFERRED;

-- All unresolved external iMessage sends compete for ONE slot.
CREATE UNIQUE INDEX imsg_one_unresolved_external_send
ON imsg_reply_actions(origin_id)
WHERE purpose IN ('progress', 'final', 'fallback', 'expiry')
  AND status IN ('claimed', 'unknown');

-- Lifetime one-shot actions.
CREATE UNIQUE INDEX imsg_one_fallback
ON imsg_reply_actions(origin_id)
WHERE purpose = 'fallback';

CREATE UNIQUE INDEX imsg_one_expiry
ON imsg_reply_actions(origin_id)
WHERE purpose = 'expiry';

CREATE UNIQUE INDEX imsg_one_mirror
ON imsg_reply_actions(origin_id)
WHERE purpose = 'mirror';

CREATE INDEX imsg_open_expiry
ON imsg_reply_obligations(expires_at)
WHERE resolution = 'open';

CREATE INDEX imsg_open_episode
ON imsg_reply_obligations(episode_key)
WHERE resolution = 'open';
