use super::*;

pub(in crate::services::discord) fn stamp_claude_e_process_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    stamp_claude_e_process_if_matches_identity_in_root(&root, state, expected)
}

pub(in crate::services::discord::inflight) fn stamp_claude_e_process_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Some(mut on_disk) = load_inflight_state_unlocked(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    if on_disk.rebind_origin || !expected.matches_state(&on_disk) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            snapshot_identity = ?expected,
            durable_identity = ?InflightTurnIdentity::from_state(&on_disk),
            "ClaudeE process-evidence stamp skipped because durable row authority changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }

    on_disk.runtime_kind = Some(RuntimeHandoffKind::ClaudeEAdapter);
    on_disk.tmux_session_name = None;
    on_disk.output_path = state.output_path.clone();
    on_disk.input_fifo_path = None;
    on_disk.last_offset = state.last_offset;
    on_disk.claude_e_pid = state.claude_e_pid;
    on_disk.claude_e_process_starttime = state.claude_e_process_starttime;
    on_disk.claude_e_macos_lstart_hash = state.claude_e_macos_lstart_hash;
    match persist_under_lock(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight.rs:stamp_claude_e_process_if_matches_identity_in_root",
    ) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                error = %error,
                "ClaudeE process-evidence stamp failed; leaving durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}
