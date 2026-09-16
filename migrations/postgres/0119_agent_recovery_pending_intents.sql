-- Keep takeover/restore intent fenced until runtime launch acknowledges it.
ALTER TABLE agent_recovery_channel_state DROP CONSTRAINT chk_arcs_status;
ALTER TABLE agent_recovery_channel_state ADD CONSTRAINT chk_arcs_status CHECK (
    status IN ('owner', 'takeover_pending', 'fallback_running', 'fallback_done',
               'restore_pending', 'restored', 'aborted')
);
ALTER TABLE agent_recovery_channel_state ADD COLUMN recovery_context JSONB;
COMMENT ON COLUMN agent_recovery_channel_state.recovery_context IS
    'Immutable owner/fallback provider and workspace binding for an active recovery lease.';
