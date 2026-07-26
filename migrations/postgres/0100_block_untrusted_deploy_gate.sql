-- #4898: deploy-gate remains unavailable until trusted typed deployment
-- evidence is implemented. Keep this validated constraint authoritative so
-- legacy binaries and other nodes cannot persist an unsupported declaration.
-- A future capability rollout must explicitly replace or remove this constraint
-- in the same migration sequence that introduces trusted evidence authority.
ALTER TABLE auto_queue_entries
    ADD CONSTRAINT auto_queue_entries_deploy_gate_unavailable_check
    CHECK (
        phase_gate_kind IS NULL
        OR lower(btrim(phase_gate_kind, E' \t\n\r\f\v')) <> 'deploy-gate'
    );
