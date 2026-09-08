-- no-transaction
-- #5320 slice 1: give terminal intake_outbox rows a time-ordered key so a later
-- retention scan can walk them in (updated_at, id) order. This file adds no
-- delete behaviour and nothing reads the index yet. The four terminal states
-- are enumerated rather than negated: a state added later must default to
-- being retained, not to being swept.
--
-- Header conventions follow 0108. CONCURRENTLY must run outside a transaction,
-- this file stays at one executable statement, and a conditional existence
-- clause is intentionally omitted so a rerun over an INVALID index hard-fails
-- instead of recording the migration over an unusable index. The PR body
-- carries the apply/rollback and INVALID-index recovery procedure.
CREATE INDEX CONCURRENTLY idx_intake_outbox_terminal_retention
    ON intake_outbox (updated_at, id)
    WHERE status IN ('done', 'unknown', 'failed_pre_accept', 'failed_post_accept');
