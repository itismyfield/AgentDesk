use super::super::*;

fn stamp_process_identity_fields(inflight_state: &mut InflightTurnState, pid: u32) {
    if pid == 0 {
        return;
    }
    let process_identity = crate::services::process::ProcessIdentity::capture(pid);
    inflight_state.claude_e_pid = Some(pid);
    inflight_state.claude_e_process_starttime = process_identity.persisted_starttime();
    inflight_state.claude_e_macos_lstart_hash = process_identity.persisted_macos_lstart_hash();
}

pub(super) fn stamp_process_evidence(
    inflight_state: &mut InflightTurnState,
    output_path: String,
    last_offset: u64,
    pid: u32,
    state_dirty: bool,
) -> (bool, crate::services::discord::inflight::GuardedSaveOutcome) {
    let expected_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(inflight_state);
    inflight_state.runtime_kind =
        Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeEAdapter);
    inflight_state.tmux_session_name = None;
    inflight_state.output_path = Some(output_path);
    inflight_state.input_fifo_path = None;
    inflight_state.last_offset = last_offset;
    stamp_process_identity_fields(inflight_state, pid);
    let outcome = crate::services::discord::inflight::stamp_claude_e_process_if_matches_identity(
        inflight_state,
        &expected_identity,
    );
    (
        super::guarded_save::tmux_ready_state_dirty_after_guarded_save(state_dirty, Some(outcome)),
        outcome,
    )
}

#[cfg(test)]
mod tests {
    use super::stamp_process_identity_fields;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;

    fn state_with_process_evidence() -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            45_960,
            None,
            7,
            8,
            9,
            "prompt".to_string(),
            Some("session".to_string()),
            None,
            Some("/tmp/claude-e-output".to_string()),
            None,
            11,
        );
        state.claude_e_pid = Some(42);
        state.claude_e_process_starttime = Some(43);
        state.claude_e_macos_lstart_hash = Some(44);
        state
    }

    #[test]
    fn recovery_pid_sentinel_preserves_existing_process_evidence() {
        let mut state = state_with_process_evidence();

        stamp_process_identity_fields(&mut state, 0);

        assert_eq!(state.claude_e_pid, Some(42));
        assert_eq!(state.claude_e_process_starttime, Some(43));
        assert_eq!(state.claude_e_macos_lstart_hash, Some(44));
    }
}
