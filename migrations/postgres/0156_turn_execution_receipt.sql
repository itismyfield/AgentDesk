-- Execution receipt on the existing `turns` row: which accepted inputs (origin ids) the
-- execution consumed and the terminal it actually reached, written apart from analytics.
--
-- Existing rows keep an empty origin list and a NULL terminal, so no earlier row reads as a
-- confirmed terminal. `turns_archive` carries the same columns so retention keeps receipts.

ALTER TABLE turns
    ADD COLUMN consumed_origin_ids TEXT[] NOT NULL DEFAULT '{}',
    ADD COLUMN terminal_outcome TEXT,
    ADD COLUMN terminal_confirmed_at TIMESTAMPTZ,
    ADD CONSTRAINT turns_terminal_outcome_check CHECK (
        terminal_outcome IS NULL
        OR terminal_outcome IN ('completed', 'cancelled', 'failed', 'empty')
    ),
    ADD CONSTRAINT turns_terminal_receipt_pair_check CHECK (
        (terminal_outcome IS NULL) = (terminal_confirmed_at IS NULL)
    );

CREATE INDEX IF NOT EXISTS idx_turns_consumed_origin_ids
    ON turns USING GIN (consumed_origin_ids);

ALTER TABLE turns_archive
    ADD COLUMN consumed_origin_ids TEXT[] NOT NULL DEFAULT '{}',
    ADD COLUMN terminal_outcome TEXT,
    ADD COLUMN terminal_confirmed_at TIMESTAMPTZ,
    ADD CONSTRAINT turns_archive_terminal_outcome_check CHECK (
        terminal_outcome IS NULL
        OR terminal_outcome IN ('completed', 'cancelled', 'failed', 'empty')
    ),
    ADD CONSTRAINT turns_archive_terminal_receipt_pair_check CHECK (
        (terminal_outcome IS NULL) = (terminal_confirmed_at IS NULL)
    );

CREATE INDEX IF NOT EXISTS idx_turns_archive_consumed_origin_ids
    ON turns_archive USING GIN (consumed_origin_ids);
