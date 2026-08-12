-- #5071 T2-W S-R1: add the durable dispatch clock and the official terminal
-- `unknown` status before any writer can emit either state.
--
-- SQLx runs this migration in one transaction. ALTER TABLE takes ACCESS
-- EXCLUSIVE and retains it through commit, including time spent waiting for
-- later statements or migration bookkeeping. The nullable column has no
-- default, so PostgreSQL does not rewrite the heap. Both replacement CHECKs
-- are NOT VALID, so this migration performs no validation scan; they still
-- constrain every new or updated row immediately.
--
-- This slice does not rebuild or change the five-state open-route index. A
-- separate rollout gate must validate these constraints, or prove there are
-- no violating rows fail-closed, before dispatched/unknown authority starts.
ALTER TABLE intake_outbox
    ADD COLUMN dispatched_at TIMESTAMPTZ;

ALTER TABLE intake_outbox
    DROP CONSTRAINT intake_outbox_status_check;

ALTER TABLE intake_outbox
    ADD CONSTRAINT intake_outbox_status_check CHECK (status IN (
        'pending',
        'claimed',
        'accepted',
        'spawned',
        'dispatched',
        'unknown',
        'done',
        'failed_pre_accept',
        'failed_post_accept'
    )) NOT VALID;

ALTER TABLE intake_outbox
    ADD CONSTRAINT intake_outbox_dispatched_requires_clock
    CHECK (status <> 'dispatched' OR dispatched_at IS NOT NULL) NOT VALID;
