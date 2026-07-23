use super::*;

pub(in crate::services::discord) fn stamp_runtime_handoff_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    stamp_runtime_handoff_if_matches_identity_in_root(&root, state, expected, caller)
}

pub(in crate::services::discord::inflight) fn stamp_runtime_handoff_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    caller: &'static str,
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
    let durable = InflightTurnIdentity::from_state(&on_disk);
    if expected.user_msg_id == 0 && expected.turn_start_offset.is_none() {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            "runtime-handoff stamp skipped because offsetless id-0 snapshot cannot safely match a durable row"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin || !expected.matches_state(&on_disk)
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            durable_restart_mode = ?on_disk.restart_mode,
            durable_rebind_origin = on_disk.rebind_origin,
            "runtime-handoff stamp skipped because durable row authority changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if expected.tmux_session_name.is_some() && state.tmux_session_name != expected.tmux_session_name
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            caller,
            snapshot_tmux_session_name = ?expected.tmux_session_name,
            requested_tmux_session_name = ?state.tmux_session_name,
            "runtime-handoff stamp skipped because an established runtime session changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }

    on_disk.runtime_kind = state.runtime_kind;
    on_disk.tmux_session_name = state.tmux_session_name.clone();
    on_disk.output_path = state.output_path.clone();
    on_disk.input_fifo_path = state.input_fifo_path.clone();
    on_disk.session_id = state.session_id.clone();
    on_disk.last_offset = state.last_offset;
    on_disk.watcher_owner_channel_id = state.watcher_owner_channel_id;
    on_disk.set_relay_owner_kind(state.effective_relay_owner_kind());
    match persist_under_lock(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight.rs:stamp_runtime_handoff_if_matches_identity_in_root",
    ) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                caller,
                error = %error,
                "runtime-handoff stamp failed; leaving durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_seed(
        provider: ProviderKind,
        channel_id: u64,
        tmux_session_name: Option<&str>,
    ) -> InflightTurnState {
        InflightTurnState::new(
            provider,
            channel_id,
            Some("adk-4259-r2".to_string()),
            343_742_347_365_974_026,
            77_010,
            18,
            "runtime handoff".to_string(),
            Some("provider-session-before-handoff".to_string()),
            tmux_session_name.map(str::to_string),
            Some("/seeded/runtime-output.jsonl".to_string()),
            None,
            512,
        )
    }

    fn load(root: &Path, provider: &ProviderKind, channel_id: u64) -> InflightTurnState {
        let path = inflight_state_path(root, provider, channel_id);
        serde_json::from_str(&std::fs::read_to_string(path).expect("read inflight row"))
            .expect("parse inflight row")
    }

    #[test]
    fn runtime_first_stamp_supports_process_claude_tui_and_codex_tui() {
        for (index, provider, runtime_kind, session_name) in [
            (
                0,
                ProviderKind::Claude,
                RuntimeHandoffKind::ProcessBackend,
                "claude-process-session",
            ),
            (
                1,
                ProviderKind::Claude,
                RuntimeHandoffKind::ClaudeTui,
                "AgentDesk-claude-adk-4259-r2",
            ),
            (
                2,
                ProviderKind::Codex,
                RuntimeHandoffKind::CodexTui,
                "AgentDesk-codex-adk-4259-r2",
            ),
        ] {
            let root = tempfile::tempdir().expect("runtime root");
            let channel_id = 42_592_100 + index;
            let seed = runtime_seed(provider.clone(), channel_id, None);
            save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
            let expected = InflightTurnIdentity::from_state(&seed);

            let mut stamp = seed.clone();
            stamp.runtime_kind = Some(runtime_kind);
            stamp.tmux_session_name = Some(session_name.to_string());
            stamp.output_path = Some(format!("/runtime/{session_name}.jsonl"));
            stamp.input_fifo_path = matches!(runtime_kind, RuntimeHandoffKind::ClaudeTui)
                .then(|| format!("/runtime/{session_name}.input"));
            stamp.session_id = Some(format!("provider-session-{index}"));
            stamp.last_offset = 4096;
            stamp.watcher_owner_channel_id = Some(channel_id + 100);
            stamp.set_relay_owner_kind(RelayOwnerKind::Watcher);

            assert_eq!(
                stamp_runtime_handoff_if_matches_identity_in_root(
                    root.path(),
                    &stamp,
                    &expected,
                    "test::runtime_first_stamp",
                ),
                GuardedSaveOutcome::Saved,
            );
            let persisted = load(root.path(), &provider, channel_id);
            assert_eq!(persisted.runtime_kind, Some(runtime_kind));
            assert_eq!(persisted.tmux_session_name.as_deref(), Some(session_name));
            assert_eq!(persisted.output_path, stamp.output_path);
            assert_eq!(persisted.input_fifo_path, stamp.input_fifo_path);
            assert_eq!(persisted.session_id, stamp.session_id);
            assert_eq!(persisted.last_offset, 4096);
            assert_eq!(persisted.watcher_owner_channel_id, Some(channel_id + 100));
            assert_eq!(
                persisted.effective_relay_owner_kind(),
                RelayOwnerKind::Watcher
            );
        }
    }

    #[test]
    fn runtime_stamp_accepts_same_session_restamp_and_rejects_changed_session() {
        let root = tempfile::tempdir().expect("runtime root");
        let provider = ProviderKind::Codex;
        let channel_id = 42_592_200;
        let seed = runtime_seed(provider.clone(), channel_id, Some("AgentDesk-codex-stable"));
        save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
        let expected = InflightTurnIdentity::from_state(&seed);

        let mut same_session = seed.clone();
        same_session.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        same_session.output_path = Some("/runtime/restamped-rollout.jsonl".to_string());
        same_session.last_offset = 2048;
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &same_session,
                &expected,
                "test::same_session_restamp",
            ),
            GuardedSaveOutcome::Saved,
        );

        let persisted = load(root.path(), &provider, channel_id);
        let persisted_expected = InflightTurnIdentity::from_state(&persisted);
        let mut changed_session = persisted.clone();
        changed_session.tmux_session_name = Some("AgentDesk-codex-different".to_string());
        changed_session.output_path = Some("/runtime/should-not-land.jsonl".to_string());
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &changed_session,
                &persisted_expected,
                "test::changed_session_rejected",
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        let preserved = load(root.path(), &provider, channel_id);
        assert_eq!(
            preserved.tmux_session_name.as_deref(),
            Some("AgentDesk-codex-stable")
        );
        assert_eq!(
            preserved.output_path.as_deref(),
            Some("/runtime/restamped-rollout.jsonl")
        );
    }

    #[test]
    fn runtime_stamp_never_creates_or_overwrites_unowned_rows() {
        let root = tempfile::tempdir().expect("runtime root");
        let provider = ProviderKind::Claude;
        let channel_id = 42_592_300;
        let seed = runtime_seed(provider.clone(), channel_id, None);
        let expected = InflightTurnIdentity::from_state(&seed);
        let mut stamp = seed.clone();
        stamp.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        stamp.tmux_session_name = Some("AgentDesk-claude-r2".to_string());

        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &stamp,
                &expected,
                "test::missing_row",
            ),
            GuardedSaveOutcome::Missing,
        );

        let mut newer = seed.clone();
        newer.user_msg_id = 99_999;
        newer.output_path = Some("/runtime/newer-turn.jsonl".to_string());
        save_inflight_state_in_root(root.path(), &newer).expect("seed re-owned row");
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &stamp,
                &expected,
                "test::concurrent_reowner",
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        let preserved = load(root.path(), &provider, channel_id);
        assert_eq!(preserved.user_msg_id, 99_999);
        assert_eq!(
            preserved.output_path.as_deref(),
            Some("/runtime/newer-turn.jsonl")
        );
    }

    #[test]
    fn runtime_stamp_fails_closed_for_ambiguous_or_reserved_authority() {
        let provider = ProviderKind::Codex;
        for (index, mutate) in ["id0", "restart", "rebind"].into_iter().enumerate() {
            let root = tempfile::tempdir().expect("runtime root");
            let channel_id = 42_592_400 + index as u64;
            let mut seed = runtime_seed(provider.clone(), channel_id, None);
            match mutate {
                "id0" => {
                    seed.user_msg_id = 0;
                    seed.turn_start_offset = None;
                }
                "restart" => seed.set_restart_mode(InflightRestartMode::DrainRestart),
                "rebind" => seed.rebind_origin = true,
                _ => unreachable!(),
            }
            save_inflight_state_in_root(root.path(), &seed).expect("seed reserved row");
            let expected = InflightTurnIdentity::from_state(&seed);
            let mut stamp = seed.clone();
            stamp.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
            stamp.tmux_session_name = Some("AgentDesk-codex-r2".to_string());

            assert_eq!(
                stamp_runtime_handoff_if_matches_identity_in_root(
                    root.path(),
                    &stamp,
                    &expected,
                    "test::reserved_authority",
                ),
                GuardedSaveOutcome::IdentityMismatch,
                "{mutate} authority must fail closed",
            );
            let preserved = load(root.path(), &provider, channel_id);
            assert_eq!(preserved.runtime_kind, seed.runtime_kind);
            assert_eq!(preserved.tmux_session_name, seed.tmux_session_name);
        }
    }

    #[test]
    fn runtime_stamp_commits_only_final_standby_or_watcher_owner_decision() {
        let root = tempfile::tempdir().expect("runtime root");
        let provider = ProviderKind::Claude;
        let channel_id = 42_592_500;
        let seed = runtime_seed(provider.clone(), channel_id, None);
        save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
        let expected = InflightTurnIdentity::from_state(&seed);

        let mut standby = seed.clone();
        standby.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        standby.tmux_session_name = Some("AgentDesk-claude-owner-r2".to_string());
        standby.set_relay_owner_kind(RelayOwnerKind::StandbyRelay);
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &standby,
                &expected,
                "test::standby_owner",
            ),
            GuardedSaveOutcome::Saved,
        );
        let persisted_standby = load(root.path(), &provider, channel_id);
        assert_eq!(
            persisted_standby.effective_relay_owner_kind(),
            RelayOwnerKind::StandbyRelay
        );

        let standby_expected = InflightTurnIdentity::from_state(&persisted_standby);
        let mut watcher = persisted_standby.clone();
        watcher.set_relay_owner_kind(RelayOwnerKind::Watcher);
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &watcher,
                &standby_expected,
                "test::watcher_owner",
            ),
            GuardedSaveOutcome::Saved,
        );
        let persisted_watcher = load(root.path(), &provider, channel_id);
        assert_eq!(
            persisted_watcher.effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );
    }
}
