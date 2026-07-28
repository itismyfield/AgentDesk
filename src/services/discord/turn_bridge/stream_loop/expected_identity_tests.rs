use super::refresh_stream_tick_expected_identity_after_handoff;
use crate::services::discord::inflight::{
    GuardedSaveOutcome, InflightTurnIdentity, InflightTurnState, load_inflight_state,
    save_inflight_state, stamp_runtime_handoff_if_matches_identity,
};
use crate::services::discord::turn_bridge::detached_current_msg_id_from_durable;
use crate::services::discord::turn_bridge::stream_tick::guarded_persist::persist_stream_tick_state;
use crate::services::provider::ProviderKind;
use serenity::model::id::ChannelId;

fn owner_state(channel_id: u64, user_msg_id: u64, tmux_session: Option<&str>) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Codex,
        channel_id,
        Some("adk-stream-handoff".to_string()),
        343_742_347_365_974_026,
        user_msg_id,
        18,
        "user prompt".to_string(),
        Some("session".to_string()),
        tmux_session.map(str::to_string),
        Some("/tmp/AgentDesk-codex-stream-handoff.jsonl".to_string()),
        Some("/tmp/AgentDesk-codex-stream-handoff.input".to_string()),
        512,
    );
    state.last_offset = 512;
    state
}

fn with_runtime_root(test: impl FnOnce()) {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    test();
}

#[test]
fn saved_tmux_ready_first_fill_recaptures_and_allows_next_stream_tick_flush() {
    with_runtime_root(|| {
        let channel = ChannelId::new(4_836_001);
        let mut state = owner_state(channel.get(), 77_010, None);
        save_inflight_state(&state).expect("seed pre-handoff row");
        let mut persisted_baseline =
            load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted baseline");
        state.clone_from(&persisted_baseline);
        let mut expected = InflightTurnIdentity::from_state(&persisted_baseline);

        state.full_response = "answer already buffered before handoff".to_string();
        state.tmux_session_name = Some("AgentDesk-codex-handoff-ready".to_string());
        state.last_offset = 1_024;
        let handoff_outcome = stamp_runtime_handoff_if_matches_identity(
            (&persisted_baseline, &mut state),
            &expected,
            "turn_bridge::stream_loop::saved_handoff_test",
        );
        assert_eq!(handoff_outcome, GuardedSaveOutcome::Saved);
        let handoff_row =
            load_inflight_state(&ProviderKind::Codex, channel.get()).expect("handoff row");
        assert_eq!(
            handoff_row.full_response,
            "answer already buffered before handoff"
        );
        assert_eq!(
            serde_json::to_value(&state).expect("serialize adopted handoff row"),
            serde_json::to_value(&handoff_row).expect("serialize durable handoff row"),
        );
        assert_eq!(handoff_row.tmux_session_name, state.tmux_session_name);
        refresh_stream_tick_expected_identity_after_handoff(
            &mut expected,
            &mut persisted_baseline,
            &state,
            Some(handoff_outcome),
        );
        assert_eq!(
            serde_json::to_value(&persisted_baseline).expect("serialize refreshed baseline"),
            serde_json::to_value(&handoff_row).expect("serialize durable handoff row"),
        );

        let mut expected_current_message = (state.current_msg_id, state.current_msg_len);
        let mut current_msg_id = detached_current_msg_id_from_durable(state.current_msg_id);
        assert_eq!(
            persist_stream_tick_state(
                &mut persisted_baseline,
                &mut state,
                &expected,
                &mut expected_current_message,
                &mut current_msg_id,
                channel,
                "turn_bridge::stream_loop::saved_handoff_test",
            ),
            GuardedSaveOutcome::Saved
        );
        let persisted =
            load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
        assert_eq!(
            persisted.full_response,
            "answer already buffered before handoff"
        );
        assert_eq!(persisted.last_offset, 1_024);
    });
}

#[test]
fn identity_mismatch_handoff_does_not_recapture_or_authorize_stream_tick_flush() {
    with_runtime_root(|| {
        let channel = ChannelId::new(4_836_002);
        let mut stale = owner_state(channel.get(), 77_010, None);
        let mut expected = InflightTurnIdentity::from_state(&stale);
        let mut persisted_baseline = stale.clone();
        stale.tmux_session_name = Some("AgentDesk-codex-stale-handoff".to_string());

        let mut successor = owner_state(channel.get(), 99_999, Some("AgentDesk-codex-successor"));
        successor.full_response = "successor answer".to_string();
        successor.last_offset = 8_192;
        save_inflight_state(&successor).expect("seed successor row");

        refresh_stream_tick_expected_identity_after_handoff(
            &mut expected,
            &mut persisted_baseline,
            &stale,
            Some(GuardedSaveOutcome::IdentityMismatch),
        );
        assert_eq!(expected.tmux_session_name, None);

        stale.full_response = "stale answer".to_string();
        stale.last_offset = 1_024;
        let mut expected_current_message = (stale.current_msg_id, stale.current_msg_len);
        let mut current_msg_id = detached_current_msg_id_from_durable(stale.current_msg_id);
        assert_eq!(
            persist_stream_tick_state(
                &mut persisted_baseline,
                &mut stale,
                &expected,
                &mut expected_current_message,
                &mut current_msg_id,
                channel,
                "turn_bridge::stream_loop::mismatched_handoff_test",
            ),
            GuardedSaveOutcome::IdentityMismatch
        );
        let persisted =
            load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
        assert_eq!(persisted.user_msg_id, 99_999);
        assert_eq!(persisted.full_response, "successor answer");
        assert_eq!(persisted.last_offset, 8_192);
    });
}

#[test]
fn no_handoff_keeps_original_expected_identity_and_stream_tick_behavior() {
    with_runtime_root(|| {
        let channel = ChannelId::new(4_836_003);
        let mut state = owner_state(
            channel.get(),
            77_010,
            Some("AgentDesk-codex-existing-session"),
        );
        save_inflight_state(&state).expect("seed owner row");
        let expected = InflightTurnIdentity::from_state(&state);
        let mut persisted_baseline = state.clone();

        state.full_response = "ordinary stream answer".to_string();
        state.last_offset = 2_048;
        let mut expected_current_message = (state.current_msg_id, state.current_msg_len);
        let mut current_msg_id = detached_current_msg_id_from_durable(state.current_msg_id);
        assert_eq!(
            persist_stream_tick_state(
                &mut persisted_baseline,
                &mut state,
                &expected,
                &mut expected_current_message,
                &mut current_msg_id,
                channel,
                "turn_bridge::stream_loop::no_handoff_test",
            ),
            GuardedSaveOutcome::Saved
        );
        let persisted =
            load_inflight_state(&ProviderKind::Codex, channel.get()).expect("persisted row");
        assert_eq!(persisted.full_response, "ordinary stream answer");
        assert_eq!(persisted.last_offset, 2_048);
    });
}
