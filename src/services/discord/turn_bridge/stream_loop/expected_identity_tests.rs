use super::exit_reconcile::RETAINED_STREAM_RETRY_BACKOFF;
use super::tool_arms::{StreamToolArmOutcome, reconcile_exact_stream_frame_after_tool_outcome};
use super::{
    refresh_stream_tick_expected_identity_after_handoff, retained_stream_retry_backoff,
    should_exit_completed_turn_on_cancel, stream_loop_should_continue,
};
use crate::services::agent_protocol::StreamMessage;
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
fn done_runtime_handoff_retry_survives_expired_terminal_drain() {
    let now = std::time::Instant::now();
    assert!(stream_loop_should_continue(true, None, true, false, now));
    assert!(stream_loop_should_continue(
        true,
        Some(now - std::time::Duration::from_millis(1)),
        true,
        false,
        now,
    ));
    assert!(
        !stream_loop_should_continue(true, None, false, false, now),
        "a completed turn without an exact retained handoff may exit normally",
    );
}

#[test]
fn done_terminal_tool_result_io_retry_survives_and_replays_exactly_once() {
    let now = std::time::Instant::now();
    let mut pending = std::collections::VecDeque::new();
    let mut tool_retry_retained = false;
    let exact_frame = StreamMessage::ToolResult {
        content: "terminal retry payload".to_string(),
        is_error: true,
        tool_use_id: Some("tool-4259-r10".to_string()),
    };

    assert!(reconcile_exact_stream_frame_after_tool_outcome(
        &mut pending,
        exact_frame,
        StreamToolArmOutcome::RetryExactFrame,
        &mut tool_retry_retained,
    ));
    assert!(stream_loop_should_continue(
        true,
        None,
        false,
        tool_retry_retained,
        now,
    ));

    let replay = pending.pop_front().expect("retained exact ToolResult");
    assert!(matches!(
        &replay,
        StreamMessage::ToolResult {
            content,
            is_error: true,
            tool_use_id: Some(tool_use_id),
        } if content == "terminal retry payload" && tool_use_id == "tool-4259-r10"
    ));
    assert!(!reconcile_exact_stream_frame_after_tool_outcome(
        &mut pending,
        replay,
        StreamToolArmOutcome::Continue,
        &mut tool_retry_retained,
    ));
    assert!(!tool_retry_retained);
    assert!(
        pending.is_empty(),
        "the successful replay must not be duplicated"
    );
    assert!(!stream_loop_should_continue(
        true,
        None,
        false,
        tool_retry_retained,
        now,
    ));
}

#[test]
fn done_cancel_waits_for_retained_terminal_tool_result_replay() {
    assert!(
        !should_exit_completed_turn_on_cancel(true, true, true),
        "post-Done cancel must not discard the retained exact ToolResult",
    );
    assert!(
        should_exit_completed_turn_on_cancel(true, true, false),
        "Done keeps its normal cancel-after-completion exit after replay settles",
    );
}

#[test]
fn disconnected_receiver_with_done_behind_retained_retry_is_backed_off() {
    let receiver_disconnected = true;
    let mut pending = std::collections::VecDeque::from([StreamMessage::Done {
        result: "done behind retry".to_string(),
        session_id: Some("session-4259-r11".to_string()),
    }]);
    let exact_frame = StreamMessage::ToolResult {
        content: "retry before disconnected Done".to_string(),
        is_error: true,
        tool_use_id: Some("tool-4259-r11".to_string()),
    };
    let mut retry_retained = false;

    assert!(receiver_disconnected);
    assert!(reconcile_exact_stream_frame_after_tool_outcome(
        &mut pending,
        exact_frame,
        StreamToolArmOutcome::RetryExactFrame,
        &mut retry_retained,
    ));
    assert!(matches!(
        pending.front(),
        Some(StreamMessage::ToolResult { .. })
    ));
    assert!(matches!(pending.get(1), Some(StreamMessage::Done { .. })));
    assert_eq!(
        retained_stream_retry_backoff(false, retry_retained),
        RETAINED_STREAM_RETRY_BACKOFF,
        "every retained guarded retry must yield even before queued Done is reached",
    );
}

#[test]
fn deterministic_tool_divergence_releases_done_and_cancel_without_requeue() {
    let mut pending = std::collections::VecDeque::from([StreamMessage::Done {
        result: "done after authority conflict".to_string(),
        session_id: Some("session-4259-r12-conflict".to_string()),
    }]);
    let exact_frame = StreamMessage::ToolResult {
        content: "already rejected divergent result".to_string(),
        is_error: false,
        tool_use_id: Some("tool-4259-r12-conflict".to_string()),
    };
    let mut retry_retained = true;

    assert!(!reconcile_exact_stream_frame_after_tool_outcome(
        &mut pending,
        exact_frame,
        StreamToolArmOutcome::AuthorityLost,
        &mut retry_retained,
    ));
    assert!(!retry_retained);
    assert_eq!(
        pending.len(),
        1,
        "the exact frame is neither duplicated nor requeued"
    );
    assert!(matches!(pending.front(), Some(StreamMessage::Done { .. })));
    assert!(should_exit_completed_turn_on_cancel(
        true,
        true,
        retry_retained
    ));
}

#[test]
fn disconnected_runtime_ready_retry_ahead_of_done_is_backed_off_before_done() {
    let receiver_disconnected = true;
    let pending = std::collections::VecDeque::from([
        StreamMessage::RuntimeReady {
            handoff: crate::services::agent_protocol::RuntimeHandoff::ProcessBackend {
                output_path: "/runtime/retained.jsonl".to_string(),
                session_name: "runtime-retained-r12".to_string(),
                last_offset: 4259,
            },
        },
        StreamMessage::Done {
            result: "done behind runtime retry".to_string(),
            session_id: Some("session-4259-r12".to_string()),
        },
    ]);

    assert!(receiver_disconnected);
    assert!(matches!(
        pending.front(),
        Some(StreamMessage::RuntimeReady { .. })
    ));
    assert!(matches!(pending.get(1), Some(StreamMessage::Done { .. })));
    assert_eq!(
        retained_stream_retry_backoff(true, false),
        RETAINED_STREAM_RETRY_BACKOFF,
        "runtime retry must yield even before Done/disconnect changes loop state",
    );
}

#[test]
fn retained_retry_policies_are_wired_to_both_cancel_boundaries_and_backoff() {
    let source = include_str!("../stream_loop.rs")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        source
            .matches("if should_exit_completed_turn_on_cancel(")
            .count(),
        2,
        "both post-Done cancel boundaries must defer retained ToolResult replay",
    );
    let retry_block = source
        .find("if runtime_handoff_retry_pending || guarded_tool_frame_retry_pending")
        .map(|start| &source[start..])
        .expect("retained retry block");
    let sleep = retry_block
        .find("tokio::time::sleep(retained_stream_retry_backoff(")
        .expect("retained retry block applies bounded backoff");
    let replay = sleep
        + retry_block[sleep..]
            .find("continue 'outer")
            .expect("exact retained frame is replayed on the next outer iteration");
    assert!(sleep < replay);
}

#[test]
fn every_later_authority_handoff_invalidates_the_bridge_entry_watcher_epoch() {
    let source = include_str!("../stream_loop.rs")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let invalidation = "*state.entry_watcher_epoch_current = false;";
    assert_eq!(source.matches(invalidation).count(), 3);

    for arm in [
        "StreamMessage::TmuxReady {",
        "StreamMessage::RuntimeReady { handoff } => {",
        "StreamMessage::ProcessReady {",
    ] {
        let arm_start = source.find(arm).expect("authority handoff arm");
        let handler = source[arm_start..]
            .find("let outcome = handle_runtime_handoff_loop_message(")
            .map(|offset| arm_start + offset)
            .expect("handoff handler");
        assert!(
            source[arm_start..handler].contains(invalidation),
            "{arm} must invalidate the bridge-entry owner epoch before mutation/retry",
        );
    }
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
