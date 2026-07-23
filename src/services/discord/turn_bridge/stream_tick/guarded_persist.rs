//! Identity-guarded persistence helpers for the periodic stream tick (#4259 R1).

use super::super::*;

pub(super) type GuardedSaveOutcome = crate::services::discord::inflight::GuardedSaveOutcome;

pub(in crate::services::discord::turn_bridge) fn persist_stream_tick_state(
    inflight_state: &InflightTurnState,
    expected: &crate::services::discord::inflight::InflightTurnIdentity,
    channel_id: ChannelId,
    caller: &'static str,
) -> GuardedSaveOutcome {
    use crate::services::discord::inflight::{
        GuardedSaveOutcome, save_inflight_state_if_identity_matches_allow_output_restamp,
    };
    let outcome = save_inflight_state_if_identity_matches_allow_output_restamp(
        inflight_state,
        expected,
        caller,
    );
    if matches!(
        outcome,
        GuardedSaveOutcome::Missing | GuardedSaveOutcome::IdentityMismatch
    ) {
        tracing::warn!(
            channel_id = channel_id.get(),
            caller,
            ?outcome,
            "stream tick guarded save skipped because durable row is no longer owned by this turn"
        );
    }
    outcome
}

pub(super) fn persist_stream_tick_heartbeat(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected: &crate::services::discord::inflight::InflightTurnIdentity,
) -> GuardedSaveOutcome {
    crate::services::discord::inflight::touch_inflight_state_if_matches_identity(
        provider,
        channel_id.get(),
        expected,
        "turn_bridge::stream_tick::long_running_heartbeat",
    )
}

pub(super) fn dirty_after_guarded_save(outcome: GuardedSaveOutcome) -> bool {
    matches!(outcome, GuardedSaveOutcome::IoError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::inflight::{
        GuardedSaveOutcome, load_inflight_state, save_inflight_state,
    };

    fn owner_state(channel_id: u64, user_msg_id: u64) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-stream-tick".to_string()),
            343_742_347_365_974_026,
            user_msg_id,
            18,
            "user prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-codex-stream-tick".to_string()),
            Some("/tmp/AgentDesk-codex-stream-tick.jsonl".to_string()),
            Some("/tmp/AgentDesk-codex-stream-tick.input".to_string()),
            512,
        );
        state.last_offset = 512;
        state
    }

    fn with_runtime_root<T>(test: impl FnOnce() -> T) -> T {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        test()
    }

    #[test]
    fn same_owner_flush_persists_and_clears_dirty() {
        with_runtime_root(|| {
            let channel = ChannelId::new(4_259_101);
            let mut state = owner_state(channel.get(), 77_010);
            save_inflight_state(&state).expect("seed owner row");
            let expected =
                crate::services::discord::inflight::InflightTurnIdentity::from_state(&state);

            state.full_response = "streamed answer".to_string();
            state.last_offset = 1_024;
            let outcome = persist_stream_tick_state(
                &state,
                &expected,
                channel,
                "turn_bridge::stream_tick::dirty_flush_test",
            );

            assert_eq!(outcome, GuardedSaveOutcome::Saved);
            assert!(!dirty_after_guarded_save(outcome));
            let persisted =
                load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
            assert_eq!(persisted.full_response, "streamed answer");
            assert_eq!(persisted.last_offset, 1_024);
        });
    }

    #[test]
    fn reowned_flush_skips_without_clobbering_or_retrying_dirty() {
        with_runtime_root(|| {
            let channel = ChannelId::new(4_259_102);
            let mut stale = owner_state(channel.get(), 77_010);
            let expected =
                crate::services::discord::inflight::InflightTurnIdentity::from_state(&stale);
            stale.full_response = "stale answer".to_string();

            let mut successor = owner_state(channel.get(), 99_999);
            successor.full_response = "new owner answer".to_string();
            successor.last_offset = 8_192;
            save_inflight_state(&successor).expect("seed successor row");

            let outcome = persist_stream_tick_state(
                &stale,
                &expected,
                channel,
                "turn_bridge::stream_tick::dirty_flush_test",
            );

            assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
            assert!(!dirty_after_guarded_save(outcome));
            let persisted =
                load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
            assert_eq!(persisted.user_msg_id, 99_999);
            assert_eq!(persisted.full_response, "new owner answer");
            assert_eq!(persisted.last_offset, 8_192);
        });
    }

    #[test]
    fn dirty_and_side_effect_transitions_follow_guarded_outcome() {
        use GuardedSaveOutcome::*;
        assert!(!dirty_after_guarded_save(Saved));
        assert!(dirty_after_guarded_save(IoError));
        assert!(!dirty_after_guarded_save(Missing));
        assert!(!dirty_after_guarded_save(IdentityMismatch));
        assert!(matches!(Saved, GuardedSaveOutcome::Saved));
        assert!(!matches!(IoError, GuardedSaveOutcome::Saved));
        assert!(!matches!(Missing, GuardedSaveOutcome::Saved));
        assert!(!matches!(IdentityMismatch, GuardedSaveOutcome::Saved));
    }

    #[test]
    fn heartbeat_touches_same_owner_but_skips_successor() {
        with_runtime_root(|| {
            let channel = ChannelId::new(4_259_103);
            let owner = owner_state(channel.get(), 77_010);
            save_inflight_state(&owner).expect("seed owner row");
            let expected =
                crate::services::discord::inflight::InflightTurnIdentity::from_state(&owner);

            assert_eq!(
                persist_stream_tick_heartbeat(&ProviderKind::Codex, channel, &expected),
                GuardedSaveOutcome::Saved
            );

            let successor = owner_state(channel.get(), 99_999);
            save_inflight_state(&successor).expect("seed successor row");
            assert_eq!(
                persist_stream_tick_heartbeat(&ProviderKind::Codex, channel, &expected),
                GuardedSaveOutcome::IdentityMismatch
            );
            let persisted =
                load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
            assert_eq!(persisted.user_msg_id, 99_999);
        });
    }
}
