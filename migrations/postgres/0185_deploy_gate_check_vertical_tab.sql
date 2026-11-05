-- 0100 wrote U+000B as E'\v': PostgreSQL 17 reads that as U+000B, 16 and older as the letter v.
-- Production applied 0100 on PG 17, so its rows already pass; a PG <=16 database fails only on U+000B-wrapped deploy-gate.
ALTER TABLE auto_queue_entries
    DROP CONSTRAINT auto_queue_entries_deploy_gate_unavailable_check,
    ADD CONSTRAINT auto_queue_entries_deploy_gate_unavailable_check
    CHECK (
        phase_gate_kind IS NULL
        OR lower(btrim(phase_gate_kind, E' \t\n\r\f\x0b')) <> 'deploy-gate'
    );
