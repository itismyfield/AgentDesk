use super::*;

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
