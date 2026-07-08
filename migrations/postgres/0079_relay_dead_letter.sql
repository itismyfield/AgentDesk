-- #4260 — Durable dead-letter sink for the silent message-loss vectors.
--
-- Three relay paths could drop a message with no durable record and no user
-- signal: (1) catch-up "too old" drops on a long restart gap, (2) the
-- intervention-queue overflow drop-oldest evict, and (3) terminal outbox
-- delivery failures. Vector 3 already has a natural dead-letter (the
-- message_outbox row itself flips to status='failed', 0001), so only vectors
-- 1 and 2 record here; vector 3 just gains a notification.
--
-- Modeled on the intake_outbox precedent (0052): a plain append-only audit
-- table. Every write is BEST-EFFORT (see db::relay_dead_letter) — a failed
-- dead-letter insert must never compound the loss by breaking the origin path.
CREATE TABLE IF NOT EXISTS relay_dead_letter (
    id          BIGSERIAL PRIMARY KEY,

    -- Loss-vector discriminator: 'catch_up_too_old' | 'queue_overflow'.
    kind        TEXT NOT NULL,

    channel_id  TEXT NOT NULL,
    -- author/message id are optional: a queue-overflow evict may carry a
    -- merged intervention with no single resolvable source message.
    author_id   TEXT,
    message_id  TEXT,

    -- The lost original content, preserved verbatim so an operator (or the
    -- user, prompted by the notice) can recover it.
    content     TEXT NOT NULL,
    reason      TEXT NOT NULL,

    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Operator triage slices by loss vector over a recency window.
CREATE INDEX IF NOT EXISTS idx_relay_dead_letter_kind_created_at
    ON relay_dead_letter (kind, created_at);

-- Per-channel recovery reads ("what did we drop for this channel?").
CREATE INDEX IF NOT EXISTS idx_relay_dead_letter_channel_created_at
    ON relay_dead_letter (channel_id, created_at);
