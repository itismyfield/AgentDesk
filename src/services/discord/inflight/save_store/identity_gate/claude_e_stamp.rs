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
    let baseline = state.baseline_state().cloned();
    let Some(provider) = requested.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, requested.channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let mut on_disk = match read_inflight_state_for_guarded_write(
        &path,
        &provider,
        requested.channel_id,
        expected,
        "stamp_claude_e_process_if_matches_identity",
    ) {
        Ok(on_disk) => on_disk,
        Err(outcome) => return outcome,
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

    let requested_process_runtime = (
        Some(RuntimeHandoffKind::ClaudeEAdapter),
        Option::<&str>::None,
        requested.output_path.as_deref(),
        Option::<&str>::None,
        requested.claude_e_pid,
        requested.claude_e_process_starttime,
        requested.claude_e_macos_lstart_hash,
    );
    let durable_process_runtime = (
        on_disk.runtime_kind,
        on_disk.tmux_session_name.as_deref(),
        on_disk.output_path.as_deref(),
        on_disk.input_fifo_path.as_deref(),
        on_disk.claude_e_pid,
        on_disk.claude_e_process_starttime,
        on_disk.claude_e_macos_lstart_hash,
    );
    let apply_process_runtime = if let Some(baseline) = baseline.as_ref() {
        let baseline_process_runtime = (
            baseline.runtime_kind,
            baseline.tmux_session_name.as_deref(),
            baseline.output_path.as_deref(),
            baseline.input_fifo_path.as_deref(),
            baseline.claude_e_pid,
            baseline.claude_e_process_starttime,
            baseline.claude_e_macos_lstart_hash,
        );
        let process_runtime_changed = requested_process_runtime != baseline_process_runtime;
        if process_runtime_changed
            && durable_process_runtime != baseline_process_runtime
            && durable_process_runtime != requested_process_runtime
        {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = requested.channel_id,
                "ClaudeE process-evidence stamp skipped because the durable process/runtime group changed"
            );
            return GuardedSaveOutcome::IdentityMismatch;
        }
        process_runtime_changed
    } else {
        true
    };

    if !merge_runtime_stamp_progress(&mut on_disk, &requested) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = requested.channel_id,
            "ClaudeE process-evidence stamp rejected because local and durable responses diverged"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if apply_process_runtime {
        on_disk.runtime_kind = Some(RuntimeHandoffKind::ClaudeEAdapter);
        on_disk.tmux_session_name = None;
        on_disk.output_path.clone_from(&requested.output_path);
        on_disk.input_fifo_path = None;
        on_disk.claude_e_pid = requested.claude_e_pid;
        on_disk.claude_e_process_starttime = requested.claude_e_process_starttime;
        on_disk.claude_e_macos_lstart_hash = requested.claude_e_macos_lstart_hash;
    }
    on_disk.last_offset = on_disk.last_offset.max(requested.last_offset);
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

    fn claude_e_request(
        baseline: &InflightTurnState,
        output_path: &str,
        pid: u32,
    ) -> InflightTurnState {
        let mut request = baseline.clone();
        request.runtime_kind = Some(RuntimeHandoffKind::ClaudeEAdapter);
        request.tmux_session_name = None;
        request.output_path = Some(output_path.to_string());
        request.input_fifo_path = None;
        request.last_offset = 4_096;
        request.claude_e_pid = Some(pid);
        request.claude_e_process_starttime = Some(u128::from(pid) + 10_000);
        request.claude_e_macos_lstart_hash = Some(u128::from(pid) + 20_000);
        request
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

    #[test]
    fn stale_claude_e_stamp_cannot_overwrite_newer_runtime_or_process_group() {
        for (case, p1_output, p1_pid, p2_output, p2_pid) in [
            (
                "runtime",
                "/runtime/claude-e-p1.jsonl",
                42_561,
                "/runtime/claude-e-p2.jsonl",
                42_561,
            ),
            (
                "process",
                "/runtime/claude-e-shared.jsonl",
                42_562,
                "/runtime/claude-e-shared.jsonl",
                42_563,
            ),
        ] {
            let root = tempfile::tempdir().expect("runtime root");
            let channel_id = if case == "runtime" {
                42_592_561
            } else {
                42_592_562
            };
            let seed = claude_e_seed(channel_id);
            save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
            let baseline = load(root.path(), channel_id);
            let expected = InflightTurnIdentity::from_state(&baseline);
            let mut stale_p1 = claude_e_request(&baseline, p1_output, p1_pid);
            let mut p2 = claude_e_request(&baseline, p2_output, p2_pid);

            assert_eq!(
                stamp_claude_e_process_if_matches_identity_in_root(
                    root.path(),
                    (&baseline, &mut p2),
                    &expected,
                ),
                GuardedSaveOutcome::Saved,
                "{case} P2 stamp",
            );
            let persisted_p2 = load(root.path(), channel_id);
            assert_eq!(
                serde_json::to_value(&p2).expect("serialize adopted P2"),
                serde_json::to_value(&persisted_p2).expect("serialize persisted P2"),
                "{case} P2 must adopt the exact persisted snapshot",
            );

            assert_eq!(
                stamp_claude_e_process_if_matches_identity_in_root(
                    root.path(),
                    (&baseline, &mut stale_p1),
                    &expected,
                ),
                GuardedSaveOutcome::IdentityMismatch,
                "stale {case} P1 must lose the group CAS",
            );
            let preserved_p2 = load(root.path(), channel_id);
            assert_eq!(
                serde_json::to_value(&preserved_p2).expect("serialize preserved P2"),
                serde_json::to_value(&persisted_p2).expect("serialize expected P2"),
                "stale {case} P1 must not overwrite P2",
            );
        }
    }

    #[test]
    fn transient_claude_e_stamp_read_error_is_retryable_and_preserves_process_frame() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_592_563;
        let seed = claude_e_seed(channel_id);
        save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
        let baseline = load(root.path(), channel_id);
        let expected = InflightTurnIdentity::from_state(&baseline);
        let mut local = claude_e_request(&baseline, "/runtime/claude-e-r9-retry.jsonl", 42_564);
        let local_before = serde_json::to_value(&local).expect("serialize process frame");

        let path = inflight_state_path(root.path(), &ProviderKind::Claude, channel_id);
        std::fs::remove_file(&path).expect("replace row with deterministic read failure");
        std::fs::create_dir(&path).expect("directory at row path forces read error");
        assert_eq!(
            stamp_claude_e_process_if_matches_identity_in_root(
                root.path(),
                (&baseline, &mut local),
                &expected,
            ),
            GuardedSaveOutcome::IoError,
        );
        assert_eq!(
            serde_json::to_value(&local).expect("serialize retained process frame"),
            local_before,
            "retryable guarded-read failure must preserve exact ClaudeE process evidence",
        );
    }

    #[test]
    fn divergent_claude_e_response_is_non_retryable_and_preserves_process_frame() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_592_564;
        let mut seed = claude_e_seed(channel_id);
        seed.full_response = "shared base".to_string();
        save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
        let baseline = load(root.path(), channel_id);
        let expected = InflightTurnIdentity::from_state(&baseline);

        let mut durable = baseline.clone();
        durable.full_response = "durable watcher branch".to_string();
        save_inflight_state_in_root(root.path(), &durable).expect("persist divergent durable row");
        let durable_before = load(root.path(), channel_id);
        let mut local = claude_e_request(&baseline, "/runtime/claude-e-r9-diverged.jsonl", 42_565);
        local.full_response = "resolved terminal branch".to_string();
        let local_before = serde_json::to_value(&local).expect("serialize process frame");

        assert_eq!(
            stamp_claude_e_process_if_matches_identity_in_root(
                root.path(),
                (&baseline, &mut local),
                &expected,
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "semantic body divergence must not retain a permanently failing process frame",
        );
        assert_eq!(serde_json::to_value(&local).unwrap(), local_before);
        assert_eq!(
            serde_json::to_value(load(root.path(), channel_id)).unwrap(),
            serde_json::to_value(durable_before).unwrap(),
        );
    }
}
