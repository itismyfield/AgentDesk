-- 0100 wrote U+000B as E'\v', which PostgreSQL 16 and older read as the letter v; \x0b is U+000B everywhere.
-- NOT VALID: existing rows are scanned later with the other deferred checks. One ALTER keeps DROP and ADD under one lock.
ALTER TABLE auto_queue_entries
    DROP CONSTRAINT auto_queue_entries_deploy_gate_unavailable_check,
    ADD CONSTRAINT auto_queue_entries_deploy_gate_unavailable_check
    CHECK (
        phase_gate_kind IS NULL
        OR lower(btrim(phase_gate_kind, E' \t\n\r\f\x0b')) <> 'deploy-gate'
    ) NOT VALID;
