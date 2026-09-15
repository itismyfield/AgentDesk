-- Fence recovery writers across process restart and concurrent watchdogs.
ALTER TABLE agent_recovery_channel_state
    ADD COLUMN generation BIGINT NOT NULL DEFAULT 0;

COMMENT ON COLUMN agent_recovery_channel_state.generation IS
    'Monotonic recovery lease fencing token; stale owner/fallback writers are rejected.';
