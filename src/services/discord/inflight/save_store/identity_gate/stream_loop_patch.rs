use super::*;
use crate::services::discord::inflight::store::persist_under_lock_with_snapshot;

fn apply_local_change_if_durable_unchanged<T: Clone + PartialEq>(
    before: &T,
    local: &T,
    durable: &mut T,
) {
    if local != before && (durable == before || durable == local) {
        durable.clone_from(local);
    }
}

fn merge_stream_response_progress(
    before: (&str, usize),
    local: (&str, usize),
    durable: (&str, usize),
) -> Option<(String, usize)> {
    let (local_body, local_offset) = local;
    let (durable_body, durable_offset) = durable;
    if local == before {
        return Some((durable_body.to_string(), durable_offset));
    }
    if durable == before {
        return Some((local_body.to_string(), local_offset));
    }
    super::merge_forward_response_progress(
        (durable_body, durable_offset),
        (local_body, local_offset),
    )
}

/// Exact durable relay-authority projection used by the stream loop's
/// lock-held visible-mutation fence.  A non-bridge projection can be a valid
/// self-handoff (watcher/standby); callers must suppress Discord mutations
/// without treating that exact projection as loss of turn lifecycle authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct StreamRelayAuthority {
    watcher_owner_channel_id: Option<u64>,
    watcher_owns_live_relay: bool,
    relay_owner_kind: RelayOwnerKind,
}

impl StreamRelayAuthority {
    pub(in crate::services::discord) fn from_state(state: &InflightTurnState) -> Self {
        Self {
            watcher_owner_channel_id: state.watcher_owner_channel_id,
            watcher_owns_live_relay: state.watcher_owns_live_relay,
            relay_owner_kind: state.effective_relay_owner_kind(),
        }
    }

    pub(in crate::services::discord) fn bridge_owns_relay(self) -> bool {
        self.relay_owner_kind == RelayOwnerKind::None
    }
}

/// Three-way merges the stream loop's owned fields onto a lock-held durable
/// row. The persisted baseline keeps unsaved local deltas visible across retry;
/// durable same-turn owner/session/tool changes win conflicts, response progress
/// merges only when the two bodies are identical or prefix-compatible, and a
/// divergent body fails closed without mutating either snapshot.
pub(in crate::services::discord) fn save_stream_tick_state_preserving_current_message_races(
    persisted_baseline: &mut InflightTurnState,
    state: &mut InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_current_msg_id: u64,
    expected_current_msg_len: usize,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_stream_tick_state_preserving_current_message_races_in_root_with_mode(
        &root,
        persisted_baseline,
        state,
        expected,
        (expected_current_msg_id, expected_current_msg_len),
        caller,
        StreamTickSaveMode::MergeConcurrentOwner,
    )
}

/// Strict precondition for a Discord-visible stream mutation.  Unlike the
/// ordinary stream merge, this refuses to apply any local delta after a
/// durable relay handoff or current-message epoch change.  The check and write
/// happen under the same inflight-row lock.
pub(in crate::services::discord) fn save_stream_tick_state_if_bridge_authority(
    persisted_baseline: &mut InflightTurnState,
    state: &mut InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_current_msg_id: u64,
    expected_current_msg_len: usize,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_stream_tick_state_preserving_current_message_races_in_root_with_mode(
        &root,
        persisted_baseline,
        state,
        expected,
        (expected_current_msg_id, expected_current_msg_len),
        caller,
        StreamTickSaveMode::StrictBridgeMutation,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamTickSaveMode {
    MergeConcurrentOwner,
    StrictBridgeMutation,
}

pub(in crate::services::discord::inflight) fn save_stream_tick_state_preserving_current_message_races_in_root(
    root: &Path,
    persisted_baseline: &mut InflightTurnState,
    state: &mut InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_current_msg_id: u64,
    expected_current_msg_len: usize,
    caller: &'static str,
) -> GuardedSaveOutcome {
    save_stream_tick_state_preserving_current_message_races_in_root_with_mode(
        root,
        persisted_baseline,
        state,
        expected,
        (expected_current_msg_id, expected_current_msg_len),
        caller,
        StreamTickSaveMode::MergeConcurrentOwner,
    )
}

pub(in crate::services::discord::inflight) fn save_stream_tick_state_if_bridge_authority_in_root(
    root: &Path,
    persisted_baseline: &mut InflightTurnState,
    state: &mut InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_current_msg_id: u64,
    expected_current_msg_len: usize,
    caller: &'static str,
) -> GuardedSaveOutcome {
    save_stream_tick_state_preserving_current_message_races_in_root_with_mode(
        root,
        persisted_baseline,
        state,
        expected,
        (expected_current_msg_id, expected_current_msg_len),
        caller,
        StreamTickSaveMode::StrictBridgeMutation,
    )
}

fn save_stream_tick_state_preserving_current_message_races_in_root_with_mode(
    root: &Path,
    persisted_baseline: &mut InflightTurnState,
    state: &mut InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_current_message: (u64, usize),
    caller: &'static str,
    mode: StreamTickSaveMode,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    if !expected.matches_state(state) || !expected.matches_state(persisted_baseline) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return GuardedSaveOutcome::IoError;
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let on_disk = match super::read_inflight_state_for_guarded_write(
        &path,
        &provider,
        state.channel_id,
        expected,
        caller,
    ) {
        Ok(on_disk) => on_disk,
        Err(outcome) => return outcome,
    };
    if expected.user_msg_id == 0 && expected.turn_start_offset.is_none() {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin || !expected.matches_state(&on_disk)
    {
        return GuardedSaveOutcome::IdentityMismatch;
    }

    let baseline_authority = StreamRelayAuthority::from_state(persisted_baseline);
    let local_authority = StreamRelayAuthority::from_state(state);
    let durable_authority = StreamRelayAuthority::from_state(&on_disk);
    let baseline_current_message = expected_current_message;
    let durable_current_message = (on_disk.current_msg_id, on_disk.current_msg_len);
    if mode == StreamTickSaveMode::StrictBridgeMutation {
        let authority_changed =
            local_authority != baseline_authority || durable_authority != baseline_authority;
        let bridge_message_epoch_changed = baseline_authority.bridge_owns_relay()
            && durable_current_message != baseline_current_message;
        if authority_changed || bridge_message_epoch_changed {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                caller,
                baseline_relay_owner = baseline_authority.relay_owner_kind.as_str(),
                local_relay_owner = local_authority.relay_owner_kind.as_str(),
                durable_relay_owner = durable_authority.relay_owner_kind.as_str(),
                expected_current_msg_id = expected_current_message.0,
                expected_current_msg_len = expected_current_message.1,
                durable_current_msg_id = on_disk.current_msg_id,
                durable_current_msg_len = on_disk.current_msg_len,
                "strict stream mutation fence rejected changed durable authority"
            );
            state.clone_from(&on_disk);
            persisted_baseline.clone_from(&on_disk);
            return GuardedSaveOutcome::IdentityMismatch;
        }

        if !baseline_authority.bridge_owns_relay() {
            // Exact watcher/standby self-handoff: adopt any same-authority
            // progress (including its current-message epoch) but never merge
            // or write bridge-local visible deltas after delegation.
            state.clone_from(&on_disk);
            persisted_baseline.clone_from(&on_disk);
            return GuardedSaveOutcome::Saved;
        }
    }

    let mut updated = on_disk.clone();
    let local_current_message = (state.current_msg_id, state.current_msg_len);
    if local_current_message != baseline_current_message
        && (durable_current_message == baseline_current_message
            || durable_current_message == local_current_message)
    {
        (updated.current_msg_id, updated.current_msg_len) = local_current_message;
    }

    let Some((merged_response, merged_response_offset)) = merge_stream_response_progress(
        (
            &persisted_baseline.full_response,
            persisted_baseline.response_sent_offset,
        ),
        (&state.full_response, state.response_sent_offset),
        (&on_disk.full_response, on_disk.response_sent_offset),
    ) else {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            caller,
            baseline_bytes = persisted_baseline.full_response.len(),
            local_bytes = state.full_response.len(),
            durable_bytes = on_disk.full_response.len(),
            "stream-tick response merge rejected: concurrent bodies are not prefix-compatible"
        );
        // This is a deterministic authority conflict, not transient storage
        // failure. Adopt the lock-held winner so callers terminate exact-frame
        // replay instead of retrying the same divergent merge forever.
        state.clone_from(&on_disk);
        persisted_baseline.clone_from(&on_disk);
        return GuardedSaveOutcome::IdentityMismatch;
    };
    updated.full_response = merged_response;
    updated.response_sent_offset = merged_response_offset;

    let local_status_changed = (state.status_message_id, state.status_panel_generation)
        != (
            persisted_baseline.status_message_id,
            persisted_baseline.status_panel_generation,
        );
    let durable_status_changed = (on_disk.status_message_id, on_disk.status_panel_generation)
        != (
            persisted_baseline.status_message_id,
            persisted_baseline.status_panel_generation,
        );
    if local_status_changed && !durable_status_changed {
        updated.status_message_id = state.status_message_id;
        updated.status_panel_generation = state.status_panel_generation;
    }

    apply_local_change_if_durable_unchanged(
        &persisted_baseline.session_id,
        &state.session_id,
        &mut updated.session_id,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.runtime_kind,
        &state.runtime_kind,
        &mut updated.runtime_kind,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.tmux_session_name,
        &state.tmux_session_name,
        &mut updated.tmux_session_name,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.output_path,
        &state.output_path,
        &mut updated.output_path,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.input_fifo_path,
        &state.input_fifo_path,
        &mut updated.input_fifo_path,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.claude_e_pid,
        &state.claude_e_pid,
        &mut updated.claude_e_pid,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.claude_e_process_starttime,
        &state.claude_e_process_starttime,
        &mut updated.claude_e_process_starttime,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.claude_e_macos_lstart_hash,
        &state.claude_e_macos_lstart_hash,
        &mut updated.claude_e_macos_lstart_hash,
    );
    updated.last_offset = updated.last_offset.max(state.last_offset);
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.current_tool_line,
        &state.current_tool_line,
        &mut updated.current_tool_line,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.prev_tool_status,
        &state.prev_tool_status,
        &mut updated.prev_tool_status,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.last_tool_name,
        &state.last_tool_name,
        &mut updated.last_tool_name,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.last_tool_summary,
        &state.last_tool_summary,
        &mut updated.last_tool_summary,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.task_notification_kind,
        &state.task_notification_kind,
        &mut updated.task_notification_kind,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.any_tool_used,
        &state.any_tool_used,
        &mut updated.any_tool_used,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.has_post_tool_text,
        &state.has_post_tool_text,
        &mut updated.has_post_tool_text,
    );
    apply_local_change_if_durable_unchanged(
        &persisted_baseline.long_running_placeholder_active,
        &state.long_running_placeholder_active,
        &mut updated.long_running_placeholder_active,
    );

    let local_owner_changed = local_authority != baseline_authority;
    let durable_owner_changed = durable_authority != baseline_authority;
    if local_owner_changed && !durable_owner_changed {
        updated.watcher_owner_channel_id = state.watcher_owner_channel_id;
        updated.watcher_owns_live_relay = state.watcher_owns_live_relay;
        updated.relay_owner_kind = state.relay_owner_kind;
    }
    for frozen_id in &state.streaming_rollover_frozen_msg_ids {
        if !updated
            .streaming_rollover_frozen_msg_ids
            .contains(frozen_id)
        {
            updated.streaming_rollover_frozen_msg_ids.push(*frozen_id);
        }
    }
    updated.ensure_finalizer_turn_id();
    if !validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:save_stream_tick_state_preserving_current_message_races_in_root",
    ) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    match persist_under_lock_with_snapshot(
        root,
        &path,
        &updated,
        "src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:save_stream_tick_state_preserving_current_message_races_in_root",
    ) {
        Ok(Some(persisted)) => {
            state.clone_from(&persisted);
            persisted_baseline.clone_from(&persisted);
            GuardedSaveOutcome::Saved
        }
        Ok(None) => GuardedSaveOutcome::IdentityMismatch,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                caller,
                error = %error,
                "stream-tick state merge failed; leaving durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

pub(in crate::services::discord) fn patch_restart_mode_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    previous_restart_mode: Option<InflightRestartMode>,
    previous_restart_generation: Option<u64>,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    patch_restart_mode_if_matches_identity_in_root(
        &root,
        state,
        expected,
        previous_restart_mode,
        previous_restart_generation,
        caller,
    )
}

fn patch_restart_mode_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    previous_restart_mode: Option<InflightRestartMode>,
    previous_restart_generation: Option<u64>,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    if state.restart_mode.is_some() != state.restart_generation.is_some()
        || previous_restart_mode.is_some() != previous_restart_generation.is_some()
    {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    let path = inflight_state_path(root, &provider, state.channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Some(mut on_disk) = load_inflight_state_unlocked(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    let durable = InflightTurnIdentity::from_state(&on_disk);
    if expected.user_msg_id == 0 && expected.turn_start_offset.is_none() {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            "stream-loop restart-mode patch skipped because offsetless id-0 snapshot cannot safely match a durable row"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.rebind_origin
        || !expected.matches_state(&on_disk)
        || on_disk.restart_mode != previous_restart_mode
        || on_disk.restart_generation != previous_restart_generation
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            expected_restart_mode = ?previous_restart_mode,
            durable_restart_mode = ?on_disk.restart_mode,
            expected_restart_generation = ?previous_restart_generation,
            durable_restart_generation = ?on_disk.restart_generation,
            durable_rebind_origin = on_disk.rebind_origin,
            "stream-loop restart-mode patch skipped because durable row identity or authority changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }

    on_disk.restart_mode = state.restart_mode;
    on_disk.restart_generation = state.restart_generation;
    match persist_under_lock(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:patch_restart_mode_if_matches_identity_in_root",
    ) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                caller,
                error = %error,
                "stream-loop restart-mode patch failed; leaving durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

pub(in crate::services::discord) fn clear_long_running_placeholder_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    clear_long_running_placeholder_if_matches_identity_in_root(
        &root, provider, channel_id, expected, caller,
    )
}

fn clear_long_running_placeholder_if_matches_identity_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Some(mut on_disk) = load_inflight_state_unlocked(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    let durable = InflightTurnIdentity::from_state(&on_disk);
    if expected.user_msg_id == 0 && expected.turn_start_offset.is_none() {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id,
            caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            "stream-loop placeholder patch skipped because offsetless id-0 snapshot cannot safely match a durable row"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin || !expected.matches_state(&on_disk)
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id,
            caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            durable_restart_mode = ?on_disk.restart_mode,
            durable_rebind_origin = on_disk.rebind_origin,
            "stream-loop placeholder patch skipped because durable row identity or authority changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }

    on_disk.long_running_placeholder_active = false;
    match persist_under_lock(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight/save_store/identity_gate/stream_loop_patch.rs:clear_long_running_placeholder_if_matches_identity_in_root",
    ) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                caller,
                error = %error,
                "stream-loop placeholder patch failed; leaving durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_state(channel_id: u64, user_msg_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-4259-r3".to_string()),
            343_742_347_365_974_026,
            user_msg_id,
            18,
            "stream loop patch".to_string(),
            Some("provider-session".to_string()),
            Some("AgentDesk-codex-4259-r3".to_string()),
            Some("/runtime/4259-r3.jsonl".to_string()),
            Some("/runtime/4259-r3.input".to_string()),
            512,
        )
    }

    fn load(root: &Path, provider: &ProviderKind, channel_id: u64) -> InflightTurnState {
        let path = inflight_state_path(root, provider, channel_id);
        serde_json::from_str(&std::fs::read_to_string(path).expect("read inflight row"))
            .expect("parse inflight row")
    }

    #[test]
    fn stream_tick_merge_keeps_first_local_chunk_while_adopting_watcher_progress() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_593_104;
        let mut baseline = owner_state(channel_id, 77_010);
        baseline.full_response = "base".to_string();
        save_inflight_state_in_root(root.path(), &baseline).expect("seed owner row");
        let expected = InflightTurnIdentity::from_state(&baseline);

        let mut local = baseline.clone();
        local.full_response = "base plus bridge chunk".to_string();
        local.current_tool_line = Some("bridge-local tool".to_string());
        local.last_offset = 640;

        let mut watcher = baseline.clone();
        watcher.full_response = "base plus".to_string();
        watcher.response_sent_offset = watcher.full_response.len();
        watcher.session_id = Some("watcher-session".to_string());
        watcher.current_tool_line = Some("watcher tool".to_string());
        watcher.set_watcher_owner_channel_id(channel_id + 1);
        watcher.set_relay_owner_kind(RelayOwnerKind::Watcher);
        watcher.last_offset = 768;
        save_inflight_state_in_root(root.path(), &watcher).expect("advance watcher row");

        assert_eq!(
            save_stream_tick_state_preserving_current_message_races_in_root(
                root.path(),
                &mut baseline,
                &mut local,
                &expected,
                0,
                0,
                "test::watcher_progress_and_first_chunk",
            ),
            GuardedSaveOutcome::Saved,
        );
        let persisted = load(root.path(), &ProviderKind::Codex, channel_id);
        assert_eq!(persisted.full_response, "base plus bridge chunk");
        assert_eq!(persisted.response_sent_offset, "base plus".len());
        assert_eq!(persisted.session_id.as_deref(), Some("watcher-session"));
        assert_eq!(persisted.current_tool_line.as_deref(), Some("watcher tool"));
        assert_eq!(
            persisted.effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );
        assert_eq!(persisted.watcher_owner_channel_id, Some(channel_id + 1));
        assert_eq!(persisted.last_offset, 768);
        assert_eq!(
            serde_json::to_value(&local).unwrap(),
            serde_json::to_value(&persisted).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&baseline).unwrap(),
            serde_json::to_value(&persisted).unwrap()
        );
    }

    #[test]
    fn strict_visible_fence_rejects_watcher_before_local_delta_and_adopts_durable() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_593_114;
        let mut baseline = owner_state(channel_id, 77_010);
        baseline.full_response = "base".to_string();
        (baseline.current_msg_id, baseline.current_msg_len) = (901, 12);
        save_inflight_state_in_root(root.path(), &baseline).expect("seed bridge row");
        let expected = InflightTurnIdentity::from_state(&baseline);

        let mut local = baseline.clone();
        local.full_response = "base plus stale bridge delta".to_string();
        (local.current_msg_id, local.current_msg_len) = (902, 18);

        let mut watcher = baseline.clone();
        watcher.full_response = "base plus watcher delta".to_string();
        watcher.response_sent_offset = watcher.full_response.len();
        watcher.set_relay_owner_kind(RelayOwnerKind::Watcher);
        save_inflight_state_in_root(root.path(), &watcher).expect("watcher takes authority");

        assert_eq!(
            save_stream_tick_state_if_bridge_authority_in_root(
                root.path(),
                &mut baseline,
                &mut local,
                &expected,
                901,
                12,
                "test::strict_watcher_authority_fence",
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        let persisted = load(root.path(), &ProviderKind::Codex, channel_id);
        assert_eq!(persisted.full_response, "base plus watcher delta");
        assert_eq!(
            (persisted.current_msg_id, persisted.current_msg_len),
            (901, 12)
        );
        assert_eq!(
            persisted.effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );
        assert_eq!(
            serde_json::to_value(&local).unwrap(),
            serde_json::to_value(&persisted).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&baseline).unwrap(),
            serde_json::to_value(&persisted).unwrap()
        );
    }

    #[test]
    fn strict_visible_fence_accepts_exact_self_delegation_and_adopts_relay_epoch() {
        for (case, relay_owner, watcher_owns_live_relay) in [
            ("watcher", RelayOwnerKind::Watcher, true),
            ("standby", RelayOwnerKind::StandbyRelay, false),
        ] {
            let root = tempfile::tempdir().expect("runtime root");
            let channel_id = if case == "watcher" {
                42_593_116
            } else {
                42_593_117
            };
            let mut baseline = owner_state(channel_id, 77_010);
            baseline.full_response = "delegated base".to_string();
            (baseline.current_msg_id, baseline.current_msg_len) = (921, 12);
            baseline.set_watcher_owner_channel_id(channel_id + 50);
            baseline.watcher_owns_live_relay = watcher_owns_live_relay;
            baseline.set_relay_owner_kind(relay_owner);
            save_inflight_state_in_root(root.path(), &baseline).expect("seed delegated row");
            let expected = InflightTurnIdentity::from_state(&baseline);

            let mut local = baseline.clone();
            local.full_response = "delegated base plus forbidden bridge delta".to_string();
            let mut relay = baseline.clone();
            relay.full_response = format!("delegated base plus {case} progress");
            relay.response_sent_offset = relay.full_response.len();
            (relay.current_msg_id, relay.current_msg_len) = (922, 24);
            save_inflight_state_in_root(root.path(), &relay).expect("advance relay row");

            assert_eq!(
                save_stream_tick_state_if_bridge_authority_in_root(
                    root.path(),
                    &mut baseline,
                    &mut local,
                    &expected,
                    921,
                    12,
                    "test::strict_exact_self_delegation",
                ),
                GuardedSaveOutcome::Saved,
                "{case} self-delegation must suppress rather than lose lifecycle authority",
            );
            let persisted = load(root.path(), &ProviderKind::Codex, channel_id);
            assert_eq!(persisted.full_response, relay.full_response);
            assert_eq!(
                (persisted.current_msg_id, persisted.current_msg_len),
                (922, 24)
            );
            assert_eq!(
                StreamRelayAuthority::from_state(&local),
                StreamRelayAuthority::from_state(&relay)
            );
            assert_eq!(
                serde_json::to_value(&local).unwrap(),
                serde_json::to_value(&persisted).unwrap()
            );
            assert_eq!(
                serde_json::to_value(&baseline).unwrap(),
                serde_json::to_value(&persisted).unwrap()
            );
        }
    }

    #[test]
    fn strict_visible_fence_rejects_foreign_delegated_owner_projection() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_593_118;
        let mut baseline = owner_state(channel_id, 77_010);
        baseline.set_watcher_owner_channel_id(channel_id + 1);
        baseline.watcher_owns_live_relay = true;
        baseline.set_relay_owner_kind(RelayOwnerKind::Watcher);
        save_inflight_state_in_root(root.path(), &baseline).expect("seed delegated row");
        let expected = InflightTurnIdentity::from_state(&baseline);
        let mut local = baseline.clone();

        let mut foreign = baseline.clone();
        foreign.set_watcher_owner_channel_id(channel_id + 2);
        save_inflight_state_in_root(root.path(), &foreign).expect("foreign watcher takes owner");

        assert_eq!(
            save_stream_tick_state_if_bridge_authority_in_root(
                root.path(),
                &mut baseline,
                &mut local,
                &expected,
                0,
                0,
                "test::strict_foreign_delegated_projection",
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        assert_eq!(
            StreamRelayAuthority::from_state(&local),
            StreamRelayAuthority::from_state(&foreign)
        );
    }

    #[test]
    fn strict_visible_fence_rejects_changed_current_message_epoch() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_593_115;
        let mut baseline = owner_state(channel_id, 77_010);
        baseline.full_response = "base".to_string();
        (baseline.current_msg_id, baseline.current_msg_len) = (911, 12);
        save_inflight_state_in_root(root.path(), &baseline).expect("seed bridge row");
        let expected = InflightTurnIdentity::from_state(&baseline);

        let mut local = baseline.clone();
        local.full_response = "base plus stale rollover".to_string();
        (local.current_msg_id, local.current_msg_len) = (912, 18);

        let mut competing = baseline.clone();
        competing.full_response = "base plus durable competitor".to_string();
        (competing.current_msg_id, competing.current_msg_len) = (913, 21);
        save_inflight_state_in_root(root.path(), &competing).expect("advance durable epoch");

        assert_eq!(
            save_stream_tick_state_if_bridge_authority_in_root(
                root.path(),
                &mut baseline,
                &mut local,
                &expected,
                911,
                12,
                "test::strict_current_message_epoch_fence",
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        let persisted = load(root.path(), &ProviderKind::Codex, channel_id);
        assert_eq!(persisted.full_response, competing.full_response);
        assert_eq!(
            (persisted.current_msg_id, persisted.current_msg_len),
            (913, 21)
        );
        let persisted_json = serde_json::to_value(&persisted).unwrap();
        assert_eq!(serde_json::to_value(&local).unwrap(), persisted_json);
        assert_eq!(
            serde_json::to_value(&baseline).unwrap(),
            serde_json::to_value(&persisted).unwrap()
        );
    }

    #[test]
    fn stream_tick_merge_preserves_unsaved_chunk_across_competing_clear_and_bind() {
        for (case, baseline_message, local_message, durable_message) in [
            ("clear", (901, 12), (901, 12), (0, 0)),
            ("bind", (0, 0), (902, 13), (903, 14)),
        ] {
            let root = tempfile::tempdir().expect("runtime root");
            let channel_id = if case == "clear" {
                42_593_105
            } else {
                42_593_106
            };
            let mut baseline = owner_state(channel_id, 77_010);
            baseline.full_response = "base".to_string();
            (baseline.current_msg_id, baseline.current_msg_len) = baseline_message;
            save_inflight_state_in_root(root.path(), &baseline).expect("seed owner row");
            let expected = InflightTurnIdentity::from_state(&baseline);

            let mut local = baseline.clone();
            local.full_response = format!("base-{case}-local-chunk");
            (local.current_msg_id, local.current_msg_len) = local_message;
            let mut competitor = baseline.clone();
            (competitor.current_msg_id, competitor.current_msg_len) = durable_message;
            save_inflight_state_in_root(root.path(), &competitor).expect("persist competitor");

            assert_eq!(
                save_stream_tick_state_preserving_current_message_races_in_root(
                    root.path(),
                    &mut baseline,
                    &mut local,
                    &expected,
                    baseline_message.0,
                    baseline_message.1,
                    "test::current_message_competitor",
                ),
                GuardedSaveOutcome::Saved,
                "{case}",
            );
            let persisted = load(root.path(), &ProviderKind::Codex, channel_id);
            assert_eq!(
                (persisted.current_msg_id, persisted.current_msg_len),
                durable_message,
                "{case}",
            );
            assert_eq!(persisted.full_response, format!("base-{case}-local-chunk"));
            assert_eq!(
                serde_json::to_value(&local).unwrap(),
                serde_json::to_value(&persisted).unwrap(),
                "{case}",
            );
        }
    }

    #[test]
    fn stream_tick_merge_fails_closed_on_non_prefix_response_divergence() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_593_107;
        let mut baseline = owner_state(channel_id, 77_010);
        baseline.full_response = "base".to_string();
        save_inflight_state_in_root(root.path(), &baseline).expect("seed owner row");
        let expected = InflightTurnIdentity::from_state(&baseline);

        let mut local = baseline.clone();
        local.full_response = "base-local".to_string();
        let mut durable = baseline.clone();
        durable.full_response = "base-durable".to_string();
        save_inflight_state_in_root(root.path(), &durable).expect("persist divergent row");
        let path = inflight_state_path(root.path(), &ProviderKind::Codex, channel_id);
        let durable_bytes_before = std::fs::read(&path).expect("read durable bytes");
        assert_eq!(
            save_stream_tick_state_preserving_current_message_races_in_root(
                root.path(),
                &mut baseline,
                &mut local,
                &expected,
                0,
                0,
                "test::non_prefix_divergence",
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        assert_eq!(
            std::fs::read(path).expect("durable survives"),
            durable_bytes_before
        );
        let durable_winner = load(root.path(), &ProviderKind::Codex, channel_id);
        assert_eq!(
            serde_json::to_value(&local).unwrap(),
            serde_json::to_value(&durable_winner).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&baseline).unwrap(),
            serde_json::to_value(&durable_winner).unwrap()
        );
    }

    #[test]
    fn stream_tick_merge_retry_is_idempotent() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_593_108;
        let mut baseline = owner_state(channel_id, 77_010);
        baseline.full_response = "base".to_string();
        save_inflight_state_in_root(root.path(), &baseline).expect("seed owner row");
        let expected = InflightTurnIdentity::from_state(&baseline);
        let mut local = baseline.clone();
        local.full_response.push_str(" local-once");

        for caller in ["test::first_attempt", "test::retry"] {
            assert_eq!(
                save_stream_tick_state_preserving_current_message_races_in_root(
                    root.path(),
                    &mut baseline,
                    &mut local,
                    &expected,
                    0,
                    0,
                    caller,
                ),
                GuardedSaveOutcome::Saved,
            );
        }
        let persisted = load(root.path(), &ProviderKind::Codex, channel_id);
        assert_eq!(persisted.full_response, "base local-once");
        assert_eq!(persisted.full_response.matches("local-once").count(), 1);
        assert_eq!(
            serde_json::to_value(&local).unwrap(),
            serde_json::to_value(&persisted).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&baseline).unwrap(),
            serde_json::to_value(&persisted).unwrap()
        );
    }

    #[test]
    fn cancel_restart_patch_first_populates_same_owner_without_rewriting_other_fields() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_593_100;
        let owner = owner_state(channel_id, 77_010);
        save_inflight_state_in_root(root.path(), &owner).expect("seed owner row");
        let expected = InflightTurnIdentity::from_state(&owner);

        let mut cancelled = owner.clone();
        cancelled.full_response = "stale in-memory response".to_string();
        cancelled.set_restart_mode(InflightRestartMode::DrainRestart);
        assert_eq!(
            patch_restart_mode_if_matches_identity_in_root(
                root.path(),
                &cancelled,
                &expected,
                None,
                None,
                "test::cancel_restart_first_populate",
            ),
            GuardedSaveOutcome::Saved,
        );

        let persisted = load(root.path(), &ProviderKind::Codex, channel_id);
        assert_eq!(
            persisted.restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
        assert_eq!(persisted.restart_generation, cancelled.restart_generation);
        assert!(persisted.full_response.is_empty());
    }

    #[test]
    fn cancel_restart_patch_rejects_reowner_and_changed_restart_authority() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_593_101;
        let owner = owner_state(channel_id, 77_010);
        let expected = InflightTurnIdentity::from_state(&owner);
        let mut cancelled = owner.clone();
        cancelled.set_restart_mode(InflightRestartMode::DrainRestart);

        let mut successor = owner_state(channel_id, 99_999);
        successor.full_response = "new owner".to_string();
        save_inflight_state_in_root(root.path(), &successor).expect("seed successor row");
        assert_eq!(
            patch_restart_mode_if_matches_identity_in_root(
                root.path(),
                &cancelled,
                &expected,
                None,
                None,
                "test::cancel_restart_reowner",
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        assert_eq!(
            load(root.path(), &ProviderKind::Codex, channel_id).user_msg_id,
            99_999
        );

        successor = owner.clone();
        successor.set_restart_mode(InflightRestartMode::HotSwapHandoff);
        save_inflight_state_in_root(root.path(), &successor).expect("seed changed authority");
        assert_eq!(
            patch_restart_mode_if_matches_identity_in_root(
                root.path(),
                &cancelled,
                &expected,
                None,
                None,
                "test::cancel_restart_authority",
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        assert_eq!(
            load(root.path(), &ProviderKind::Codex, channel_id).restart_mode,
            Some(InflightRestartMode::HotSwapHandoff),
        );
    }

    #[test]
    fn stream_loop_patches_never_create_missing_rows() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_593_102;
        let owner = owner_state(channel_id, 77_010);
        let expected = InflightTurnIdentity::from_state(&owner);
        let mut cancelled = owner.clone();
        cancelled.set_restart_mode(InflightRestartMode::DrainRestart);

        assert_eq!(
            patch_restart_mode_if_matches_identity_in_root(
                root.path(),
                &cancelled,
                &expected,
                None,
                None,
                "test::cancel_restart_missing",
            ),
            GuardedSaveOutcome::Missing,
        );
        assert_eq!(
            clear_long_running_placeholder_if_matches_identity_in_root(
                root.path(),
                &ProviderKind::Codex,
                channel_id,
                &expected,
                "test::placeholder_missing",
            ),
            GuardedSaveOutcome::Missing,
        );
        assert!(!inflight_state_path(root.path(), &ProviderKind::Codex, channel_id).exists());
    }

    #[test]
    fn done_placeholder_patch_clears_same_owner_but_preserves_reowner() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_593_103;
        let mut owner = owner_state(channel_id, 77_010);
        owner.long_running_placeholder_active = true;
        save_inflight_state_in_root(root.path(), &owner).expect("seed owner row");
        let expected = InflightTurnIdentity::from_state(&owner);

        assert_eq!(
            clear_long_running_placeholder_if_matches_identity_in_root(
                root.path(),
                &ProviderKind::Codex,
                channel_id,
                &expected,
                "test::placeholder_same_owner",
            ),
            GuardedSaveOutcome::Saved,
        );
        assert!(
            !load(root.path(), &ProviderKind::Codex, channel_id).long_running_placeholder_active
        );

        let mut successor = owner_state(channel_id, 99_999);
        successor.long_running_placeholder_active = true;
        successor.full_response = "new owner".to_string();
        save_inflight_state_in_root(root.path(), &successor).expect("seed successor row");
        assert_eq!(
            clear_long_running_placeholder_if_matches_identity_in_root(
                root.path(),
                &ProviderKind::Codex,
                channel_id,
                &expected,
                "test::placeholder_reowner",
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        let persisted = load(root.path(), &ProviderKind::Codex, channel_id);
        assert_eq!(persisted.user_msg_id, 99_999);
        assert!(persisted.long_running_placeholder_active);
        assert_eq!(persisted.full_response, "new owner");
    }
}
