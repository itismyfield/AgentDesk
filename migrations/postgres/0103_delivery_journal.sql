CREATE TABLE delivery_journal_events (
    event_id UUID PRIMARY KEY,
    obligation_id UUID NOT NULL,
    attempt_id UUID,
    event_kind TEXT NOT NULL,
    event_seq SMALLINT NOT NULL,
    idempotency_key BYTEA NOT NULL,
    canonical_payload JSONB NOT NULL,
    requested_channel_id TEXT,
    returned_channel_id TEXT,
    message_id TEXT,
    observed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT delivery_journal_kind_check
        CHECK (event_kind IN ('O', 'A', 'T', 'C', 'S', 'U')),
    CONSTRAINT delivery_journal_slot_check CHECK (
        (event_kind = 'O' AND event_seq = 0) OR
        (event_kind IN ('A', 'S') AND event_seq = 1) OR
        (event_kind IN ('T', 'U') AND event_seq = 2) OR
        (event_kind = 'C' AND event_seq = 3)
    ),
    CONSTRAINT delivery_journal_attempt_check CHECK (
        (event_kind IN ('T', 'A', 'C', 'U') AND attempt_id IS NOT NULL) OR
        (event_kind IN ('O', 'S') AND attempt_id IS NULL)
    ),
    CONSTRAINT delivery_journal_transport_receipt_check CHECK (
        event_kind <> 'T' OR (
            requested_channel_id IS NOT NULL AND
            returned_channel_id IS NOT NULL AND
            message_id IS NOT NULL
        )
    ),
    CONSTRAINT delivery_journal_obligation_slot_unique
        UNIQUE (obligation_id, event_seq)
);

CREATE UNIQUE INDEX delivery_journal_single_o_a_t
    ON delivery_journal_events (obligation_id, event_kind)
    WHERE event_kind IN ('O', 'A', 'T');

CREATE UNIQUE INDEX delivery_journal_single_terminal
    ON delivery_journal_events (obligation_id)
    WHERE event_kind IN ('C', 'S', 'U');

CREATE INDEX delivery_journal_obligation_order
    ON delivery_journal_events (obligation_id, event_seq);
