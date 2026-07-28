use super::*;
use crate::services::discord::inflight::store::persist_under_lock_with_snapshot;

pub(in crate::services::discord) fn stamp_claude_e_process_if_matches_identity<
    T: GuardedStampTarget,
>(
    state: T,
    expected: &InflightTurnIdentity,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    stamp_claude_e_process_if_matches_identity_in_root(&root, state, expected)
}

pub(in crate::services::discord::inflight) fn stamp_claude_e_process_if_matches_identity_in_root<
    T: GuardedStampTarget,
>(
    root: &Path,
    state: T,
    expected: &InflightTurnIdentity,
) -> GuardedSaveOutcome {
    let requested = InflightTurnState::clone(state.local_state());
    let Some(provider) = requested.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, requested.channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Some(mut on_disk) = load_inflight_state_unlocked(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    if (expected.user_msg_id == 0 && expected.turn_start_offset.is_none())
        || on_disk.restart_mode.is_some()
        || on_disk.rebind_origin
        || !expected.matches_state(&on_disk)
    {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = requested.channel_id,
            snapshot_identity = ?expected,
            durable_identity = ?InflightTurnIdentity::from_state(&on_disk),
            "ClaudeE process-evidence stamp skipped because durable row authority changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }

    if !merge_runtime_stamp_progress(&mut on_disk, &requested) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = requested.channel_id,
            "ClaudeE process-evidence stamp deferred because local and durable responses diverged"
        );
        return GuardedSaveOutcome::IoError;
    }
    on_disk.runtime_kind = Some(RuntimeHandoffKind::ClaudeEAdapter);
    on_disk.tmux_session_name = None;
    on_disk.output_path = requested.output_path.clone();
    on_disk.input_fifo_path = None;
    on_disk.last_offset = on_disk.last_offset.max(requested.last_offset);
    on_disk.claude_e_pid = requested.claude_e_pid;
    on_disk.claude_e_process_starttime = requested.claude_e_process_starttime;
    on_disk.claude_e_macos_lstart_hash = requested.claude_e_macos_lstart_hash;
    match persist_under_lock_with_snapshot(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight.rs:stamp_claude_e_process_if_matches_identity_in_root",
    ) {
        Ok(Some(persisted)) => {
            state.adopt_persisted(persisted);
            GuardedSaveOutcome::Saved
        }
        Ok(None) => GuardedSaveOutcome::IdentityMismatch,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = requested.channel_id,
                error = %error,
                "ClaudeE process-evidence stamp failed; leaving durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_e_seed(channel_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some("adk-4259-r7".to_string()),
            343_742_347_365_974_026,
            77_010,
            18,
            "claude-e handoff".to_string(),
            Some("claude-session".to_string()),
            None,
            Some("/runtime/claude-e-before.jsonl".to_string()),
            None,
            512,
        )
    }

    fn load(root: &Path, channel_id: u64) -> InflightTurnState {
        let path = inflight_state_path(root, &ProviderKind::Claude, channel_id);
        serde_json::from_str(&std::fs::read_to_string(path).expect("read inflight row"))
            .expect("parse inflight row")
    }

    #[test]
    fn claude_e_stamp_preserves_concurrent_progress_and_adopts_exact_persisted_row() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_592_560;
        let seed = claude_e_seed(channel_id);
        save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
        let baseline = load(root.path(), channel_id);
        let expected = InflightTurnIdentity::from_state(&baseline);

        let mut durable_progress = baseline.clone();
        durable_progress.current_msg_id = 810_001;
        durable_progress.current_msg_len = 29;
        durable_progress.full_response = "watcher claude response".to_string();
        durable_progress.response_sent_offset = durable_progress.full_response.len();
        durable_progress.last_tool_name = Some("Read".to_string());
        durable_progress.last_tool_summary = Some("durable tool summary".to_string());
        durable_progress.any_tool_used = true;
        durable_progress.watcher_owner_channel_id = Some(channel_id + 1);
        durable_progress.set_relay_owner_kind(RelayOwnerKind::Watcher);
        save_inflight_state_in_root(root.path(), &durable_progress)
            .expect("advance same-turn durable progress");
        let durable_progress = load(root.path(), channel_id);

        let mut local = baseline.clone();
        local.output_path = Some("/runtime/claude-e-after.jsonl".to_string());
        local.last_offset = 4_096;
        local.claude_e_pid = Some(42_560);
        local.claude_e_process_starttime = Some(123_456);
        local.claude_e_macos_lstart_hash = Some(654_321);
        assert_eq!(
            stamp_claude_e_process_if_matches_identity_in_root(
                root.path(),
                (&baseline, &mut local),
                &expected,
            ),
            GuardedSaveOutcome::Saved,
        );

        let persisted = load(root.path(), channel_id);
        assert_eq!(
            serde_json::to_value(&local).expect("serialize adopted local row"),
            serde_json::to_value(&persisted).expect("serialize persisted row"),
        );
        assert!(persisted.save_generation > durable_progress.save_generation);
        assert_eq!(persisted.current_msg_id, 810_001);
        assert_eq!(persisted.full_response, "watcher claude response");
        assert_eq!(persisted.last_tool_name.as_deref(), Some("Read"));
        assert_eq!(
            persisted.last_tool_summary.as_deref(),
            Some("durable tool summary")
        );
        assert_eq!(persisted.watcher_owner_channel_id, Some(channel_id + 1));
        assert_eq!(
            persisted.effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );
        assert_eq!(
            persisted.runtime_kind,
            Some(RuntimeHandoffKind::ClaudeEAdapter)
        );
        assert_eq!(persisted.claude_e_pid, Some(42_560));
        assert_eq!(persisted.claude_e_process_starttime, Some(123_456));
        assert_eq!(persisted.claude_e_macos_lstart_hash, Some(654_321));
        assert_eq!(persisted.last_offset, 4_096);
    }
}
