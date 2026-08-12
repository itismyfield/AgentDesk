-- no-transaction
-- #5071 T2-W S-R1: support intake-row to journal-obligation lookup from the
-- top-level binding carried by OutputObligation events.
--
-- PostgreSQL requires CONCURRENTLY outside a transaction block. Keep this file
-- to this single executable statement. A failed concurrent build can leave an
-- INVALID index, so a conditional existence clause is intentionally omitted:
-- rerunning must hard-fail instead of letting SQLx record the migration over an
-- unusable index. Inspect pg_index.indisvalid. If INVALID, DROP INDEX
-- CONCURRENTLY idx_delivery_journal_intake_binding (or REINDEX INDEX
-- CONCURRENTLY when applicable), resolve the original failure, and rerun. If
-- valid but unrecorded, either keep it and mark 0109 applied or drop it
-- concurrently and rerun.
CREATE INDEX CONCURRENTLY idx_delivery_journal_intake_binding
    ON delivery_journal_events ((canonical_payload->>'intake_outbox_id')) WHERE event_kind='O';
