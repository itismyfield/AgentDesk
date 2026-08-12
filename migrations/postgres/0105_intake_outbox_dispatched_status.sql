-- #5071 T2-M: extend the intake lifecycle schema without changing writers.
-- No production path writes `dispatched` in this migration slice.

-- Extend the status domain to include 'dispatched' (future T2-W writer).
-- PostgreSQL requires DROP + ADD of CHECK constraint. The replacement is a strict
-- superset of the valid 0052 status domain, so every existing row is already known
-- to satisfy it. NOT VALID avoids a redundant validation scan while still enforcing
-- the replacement on every new or updated row. Validate it in a separate low-traffic
-- migration, under PostgreSQL's SHARE UPDATE EXCLUSIVE lock, before T2-W activates
-- the dispatched writer; validation is not required for correctness in this slice.
-- The transactional execution (sqlx default) means no concurrent constraint gap.
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
    )) NOT VALID;

-- sqlx 0.8+ executes this file within a single transaction unless its first line is
-- the exact `-- no-transaction` opt-out; see 0095_intake_outbox_idempotency_key_index.sql.
-- This prevents concurrent transactions from observing a state where the open-route invariant
-- (partial unique index) is missing or inconsistent with the status domain.
-- A concurrent rebuild cannot run in this transaction. Splitting only the index into a
-- `CREATE INDEX CONCURRENTLY` migration would not remove this transaction's unavoidable
-- ACCESS EXCLUSIVE ALTER lock, so the invariant-preserving replacement stays atomic here.
-- Consequently, the ACCESS EXCLUSIVE lock is held through commit and the non-concurrent
-- index build performs one full heap scan; live intake writes can block waiting for it.
-- Reusing the discriminator name 'intake_outbox_one_open_route_per_channel' keeps
-- Rust's 23505 (UNIQUE violation) classification stable in error handling (see src/db/intake_outbox.rs).
DROP INDEX intake_outbox_one_open_route_per_channel;

CREATE UNIQUE INDEX intake_outbox_one_open_route_per_channel
    ON intake_outbox (channel_id)
    WHERE status IN ('pending', 'claimed', 'accepted', 'spawned', 'dispatched');
