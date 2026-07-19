use super::super::*;

pub(super) fn stamp_process_evidence(
    inflight_state: &mut InflightTurnState,
    output_path: String,
    last_offset: u64,
    pid: u32,
    state_dirty: bool,
) -> bool {
    let expected_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(inflight_state);
    let expected_save_generation = inflight_state.save_generation;
    let process_identity = crate::services::process::ProcessIdentity::capture(pid);
    inflight_state.runtime_kind =
        Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeEAdapter);
    inflight_state.tmux_session_name = None;
    inflight_state.output_path = Some(output_path);
    inflight_state.input_fifo_path = None;
    inflight_state.last_offset = last_offset;
    inflight_state.claude_e_pid = Some(pid);
    inflight_state.claude_e_process_starttime = process_identity.persisted_starttime();
    inflight_state.claude_e_macos_lstart_hash = process_identity.persisted_macos_lstart_hash();
    let outcome =
        crate::services::discord::inflight::stamp_claude_e_process_if_matches_identity_generation(
            inflight_state,
            &expected_identity,
            expected_save_generation,
        );
    super::guarded_save::tmux_ready_state_dirty_after_guarded_save(state_dirty, Some(outcome))
}
