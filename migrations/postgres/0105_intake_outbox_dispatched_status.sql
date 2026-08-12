-- #5071 T2-M: extend the intake lifecycle schema without changing writers.
-- No production path writes `dispatched` in this migration slice.

-- Extend the status domain to include 'dispatched' (future T2-W writer).
-- PostgreSQL requires DROP + ADD of CHECK constraint; this incurs a full-table validation scan.
-- The transactional execution (sqlx default) means no concurrent window.
ALTER TABLE intake_outbox
    DROP CONSTRAINT intake_outbox_status_check;

ALTER TABLE intake_outbox
    ADD CONSTRAINT intake_outbox_status_check CHECK (status IN (
        'pending',
        'claimed',
        'accepted',
        'spawned',
        'dispatched',
        'done',
        'failed_pre_accept',
        'failed_post_accept'
    ));

-- sqlx 0.8+ executes this file within a single transaction (no `-- sqlx: no-tx` flag).
-- This prevents concurrent transactions from observing a state where the open-route invariant
-- (partial unique index) is missing or inconsistent with the status domain.
-- Reusing the discriminator name 'intake_outbox_one_open_route_per_channel' keeps
-- Rust's 23505 (UNIQUE violation) classification stable in error handling (see src/db/intake_outbox.rs).
DROP INDEX intake_outbox_one_open_route_per_channel;

CREATE UNIQUE INDEX intake_outbox_one_open_route_per_channel
    ON intake_outbox (channel_id)
    WHERE status IN ('pending', 'claimed', 'accepted', 'spawned', 'dispatched');
