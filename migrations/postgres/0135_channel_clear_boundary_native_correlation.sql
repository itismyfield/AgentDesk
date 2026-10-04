-- Native clear correlation on the channel clear boundary row.
--
-- A native clear writes `native_clear_generation` and its opaque ticket in the
-- same statement that advances `clear_generation`, and sets
-- `native_clear_resolved_at` once the clear's effect is durable. Other boundary
-- writers never touch these columns, so a later generation supersedes the ticket.
--
-- No defaults, no backfill: NULL means the row carries no native correlation.
ALTER TABLE channel_session_clear_boundaries
    ADD COLUMN IF NOT EXISTS native_clear_generation BIGINT,
    ADD COLUMN IF NOT EXISTS native_clear_ticket JSONB,
    ADD COLUMN IF NOT EXISTS native_clear_resolved_at TIMESTAMPTZ;

-- A correlation is either absent or carries both its generation and ticket.
ALTER TABLE channel_session_clear_boundaries
    ADD CONSTRAINT chk_channel_clear_boundary_native_complete CHECK (
        (native_clear_generation IS NULL
            AND native_clear_ticket IS NULL
            AND native_clear_resolved_at IS NULL)
        OR (native_clear_generation IS NOT NULL
            AND native_clear_ticket IS NOT NULL)
    );
