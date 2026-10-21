use super::*;
use crate::services::discord::inflight::{
    InflightTurnIdentity, RelayOwnerKind, StreamRelayAuthority, load_inflight_state,
    save_inflight_state,
};
use crate::services::discord::turn_bridge::stream_tick::provider_output_guard_tests::CapturingGateway;

#[tokio::test(flavor = "current_thread")]
async fn rejected_error_rewind_reloads_the_owner_and_next_tick_loses_authority() {
    let temp = tempfile::TempDir::new().expect("isolated inflight root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let channel = ChannelId::new(42_496_249);
    let provider = ProviderKind::Codex;
    let mut inflight = InflightTurnState::new(
        provider.clone(),
        channel.get(),
        Some("rewind-owner-error".into()),
        343_742_347_365_974_026,
        77_010,
        18,
        "prompt".into(),
        Some("session".into()),
        Some("rewind-owner-error-session".into()),
        Some("/tmp/rewind-owner-error.jsonl".into()),
        None,
        512,
    );
    inflight.full_response = "watcher-delivered answer".into();
    inflight.response_sent_offset = inflight.full_response.len();
    inflight.set_relay_owner_kind(RelayOwnerKind::Watcher);
    inflight.set_watcher_owner_channel_id(channel.get());
    save_inflight_state(&inflight).expect("seed watcher-owned row");
    let expected = InflightTurnIdentity::from_state(&inflight);
    let mut baseline = inflight.clone();
    let mut expected_current_message = (inflight.current_msg_id, inflight.current_msg_len);

    // Another writer takes delivery before the Error arm resets its local owner.
    let mut winner = inflight.clone();
    winner.set_relay_owner_kind(RelayOwnerKind::StandbyRelay);
    winner.set_watcher_owner_channel_id(channel.get() + 1);
    winner.full_response = "durable delivery owned by the new relay".into();
    winner.response_sent_offset = winner.full_response.len();
    save_inflight_state(&winner).expect("publish concurrent delivery owner");
    let winner_authority = StreamRelayAuthority::from_state(&winner);
    let shared = crate::services::discord::make_shared_data_for_tests();
    assert!(
        shared.pg_pool.is_none(),
        "the ordinary legacy path needs no database"
    );
    let captured = Arc::new(CapturingGateway::default());
    let gateway: Arc<dyn TurnGateway> = captured.clone();
    let cancel = CancelToken::from_persisted_turn_nonce(inflight.turn_nonce.clone());
    let mut full_response = inflight.full_response.clone();
    let (mut sent, mut confirmed) = (inflight.response_sent_offset, 0usize);
    let (mut dirty, mut panel_dirty) = (false, false);
    let (mut tool_line, mut prev_tool, mut tool_name, mut tool_summary) = (None, None, None, None);
    let (mut any_tool_used, mut post_tool_text) = (false, false);
    let mut last_edit_text = String::new();
    let (mut session, mut raw_session) = (None, None);
    let mut transcript = Vec::new();
    let (mut handshake, mut streamed, mut last_assistant) = (false, false, None);
    let (mut recovery_retry, mut open_after, mut long_running, mut retarget_after) =
        (false, None, None, None);
    let (mut cleanup_ids, mut child_ids, frozen) = (Vec::new(), Vec::new(), Vec::new());
    let (mut done, mut drain_until, mut transport_error) = (false, None, false);
    let mut error_classification = TuiErrorClassification::default();
    let (mut resume_failure, mut terminal_reset) = (false, false);
    let (mut input_tokens, mut cache_create, mut cache_read, mut output_tokens) = (0, 0, 0, 0);

    let arm_outcome = handle_stream_content_message(
        StreamContentArmMessage::Error {
            message: "ordinary provider failure".into(),
            stderr: String::new(),
        },
        StreamContentArmContext {
            shared_owned: &shared,
            gateway: &gateway,
            channel_id: channel,
            provider: &provider,
            cancel_token: &cancel,
            recovery_lease: None,
            expected_identity: &expected,
            voice_progress_playback_channel_id: None,
            watcher_owns_assistant_relay: true,
            watcher_relay_available_for_turn: true,
            standby_relay_owns_output: false,
            terminal_control_ready_observed: false,
            herdr_aborted: false,
            streaming_rollover_frozen_msg_ids: &frozen,
            context_compact_lower_bound_tokens: 0,
            context_window_tokens: 0,
            context_compact_percent: 0,
        },
        StreamContentArmState {
            state_dirty: &mut dirty,
            full_response: &mut full_response,
            current_tool_line: &mut tool_line,
            prev_tool_status: &mut prev_tool,
            last_tool_name: &mut tool_name,
            last_tool_summary: &mut tool_summary,
            any_tool_used: &mut any_tool_used,
            has_post_tool_text: &mut post_tool_text,
            response_sent_offset: &mut sent,
            last_edit_text: &mut last_edit_text,
            new_session_id: &mut session,
            new_raw_provider_session_id: &mut raw_session,
            inflight_state: &mut inflight,
            transcript_events: &mut transcript,
            session_handshake_seen: &mut handshake,
            streamed_assistant_text_this_turn: &mut streamed,
            last_assistant_text_line: &mut last_assistant,
            status_panel_dirty: &mut panel_dirty,
            recovery_retry: &mut recovery_retry,
            pending_long_running_open_after_state_save: &mut open_after,
            long_running_placeholder_active: &mut long_running,
            pending_long_running_retarget_after_state_save: &mut retarget_after,
            terminal_full_replay_cleanup_msg_ids: &mut cleanup_ids,
            active_background_child_session_ids: &mut child_ids,
            done: &mut done,
            terminal_control_drain_until: &mut drain_until,
            transport_error: &mut transport_error,
            tui_error_classification: &mut error_classification,
            resume_failure_detected: &mut resume_failure,
            bridge_confirmed_response_sent_offset: &mut confirmed,
            terminal_session_reset_required: &mut terminal_reset,
            accumulated_input_tokens: &mut input_tokens,
            accumulated_cache_create_tokens: &mut cache_create,
            accumulated_cache_read_tokens: &mut cache_read,
            accumulated_output_tokens: &mut output_tokens,
        },
    )
    .await;
    assert!(matches!(
        arm_outcome,
        StreamContentArmOutcome::ContinueDraining
    ));
    assert!(done && dirty && transport_error);
    assert!(!recovery_retry);
    assert_eq!(transcript.len(), 1, "the actual Error arm ran");
    assert_ne!(
        full_response, winner.full_response,
        "Error presentation is still a local delta"
    );
    assert_eq!(
        StreamRelayAuthority::from_state(&inflight),
        winner_authority
    );
    assert_eq!(
        inflight.full_response, winner.full_response,
        "W4 rejection reloaded the winner"
    );
    assert_eq!(
        (sent, confirmed),
        (winner.response_sent_offset, winner.response_sent_offset)
    );
    let durable_after_error = load_inflight_state(&provider, channel.get()).expect("winner row");
    assert_eq!(
        StreamRelayAuthority::from_state(&durable_after_error),
        winner_authority
    );
    assert_eq!(durable_after_error.full_response, winner.full_response);
    assert_eq!(
        durable_after_error.response_sent_offset,
        winner.response_sent_offset
    );

    let now = tokio::time::Instant::now();
    let (mut refresh, mut panel_edit, mut status_edit) = (now, now, now);
    let (mut spin_idx, mut panel_msg, mut panel_text) = (0usize, None, String::new());
    let (mut watcher_owns, mut watcher_available, mut pin, mut standby) = (true, true, None, false);
    let mut watcher_channel = channel;
    let mut frozen = frozen;
    let (mut candidate, mut created) = (None, None);
    let mut current_message = detached_current_msg_id_from_durable(inflight.current_msg_id);
    let mut first_answer_relayed = true;
    let mut tmux_offset = None;
    let mut spans =
        bridge_latency_spans::BridgeLatencySpans::starting_at(std::time::Instant::now());
    let mut generation = 0;
    let (mut heartbeat, mut long_run_heartbeat) =
        (std::time::Instant::now(), std::time::Instant::now());

    let actor_nonce = baseline.turn_nonce.clone();
    let tick_outcome = run_bridge_stream_tick(
        BridgeStreamTickContext {
            shared_owned: shared,
            gateway,
            channel_id: channel,
            provider: &provider,
            turn_id: "rewind-owner-error",
            expected_identity: &expected,
            captured_nonce: actor_nonce.as_deref(),
            status_interval: std::time::Duration::from_secs(3_600),
            single_message_panel_footer_mode: false,
            footer_owner:
                crate::services::discord::footer_view_reconciler::CompletionFooterOwner::new(
                    77_010, 0,
                ),
            status_panel_started_at: 0,
            done,
            dispatch_id: None,
            adk_session_key: None,
            adk_session_name: None,
            adk_session_info: None,
            adk_cwd: None,
            role_binding: None,
            spinner: &["|"],
            live_long_run_heartbeat_interval: std::time::Duration::from_secs(3_600),
        },
        BridgeStreamTickState {
            state_dirty: &mut dirty,
            last_session_panel_lifecycle_refresh: &mut refresh,
            status_panel_dirty: &mut panel_dirty,
            spin_idx: &mut spin_idx,
            last_status_panel_edit: &mut panel_edit,
            last_status_edit: &mut status_edit,
            status_panel_msg_id: &mut panel_msg,
            last_status_panel_text: &mut panel_text,
            watcher_owns_assistant_relay: &mut watcher_owns,
            watcher_relay_available_for_turn: &mut watcher_available,
            watcher_delivery_pin: &mut pin,
            standby_relay_owns_output: &mut standby,
            watcher_owner_channel_id: &mut watcher_channel,
            full_response: &mut full_response,
            response_sent_offset: &mut sent,
            bridge_confirmed_response_sent_offset: &mut confirmed,
            streaming_rollover_frozen_msg_ids: &mut frozen,
            current_msg_id: &mut current_message,
            expected_current_message: &mut expected_current_message,
            pending_current_message_candidate: &mut candidate,
            bridge_created_response_placeholder_msg_id: &mut created,
            last_edit_text: &mut last_edit_text,
            first_answer_relayed: &mut first_answer_relayed,
            current_tool_line: &mut tool_line,
            prev_tool_status: &mut prev_tool,
            last_tool_name: &mut tool_name,
            last_tool_summary: &mut tool_summary,
            any_tool_used: &mut any_tool_used,
            has_post_tool_text: &mut post_tool_text,
            tmux_last_offset: &mut tmux_offset,
            persisted_inflight_baseline: &mut baseline,
            inflight_state: &mut inflight,
            bridge_spans: &mut spans,
            status_panel_generation: &mut generation,
            pending_long_running_open_after_state_save: &mut open_after,
            pending_long_running_retarget_after_state_save: &mut retarget_after,
            long_running_placeholder_active: &mut long_running,
            last_adk_heartbeat: &mut heartbeat,
            last_inflight_long_run_heartbeat: &mut long_run_heartbeat,
        },
    )
    .await;
    assert_eq!(tick_outcome, StreamTickOutcome::AuthorityLost);
    assert_eq!(
        spin_idx, 0,
        "authority preflight returned before the tick body"
    );
    assert_eq!(
        full_response, winner.full_response,
        "the strict fence reloaded the durable body"
    );
    let durable_after_tick = load_inflight_state(&provider, channel.get()).expect("winner remains");
    assert_eq!(
        StreamRelayAuthority::from_state(&durable_after_tick),
        winner_authority
    );
    assert_eq!(durable_after_tick.full_response, winner.full_response);
    assert_eq!(
        durable_after_tick.response_sent_offset,
        winner.response_sent_offset
    );
    assert!(captured.sends.lock().unwrap().is_empty());
    assert!(captured.edits.lock().unwrap().is_empty());
    assert!(captured.deletes.lock().unwrap().is_empty());
}

/// A successor saved in this turn's start second shares every identity field with it; the
/// error arm's rejected rewind keeps this turn's own local instead of reloading the successor's
/// row and handing it this turn's settlement authority.
#[test]
fn rejected_error_rewind_keeps_a_successor_episode_out_of_the_local_7219() {
    let temp = tempfile::TempDir::new().expect("isolated inflight root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let channel = ChannelId::new(42_497_219);
    let provider = ProviderKind::Codex;
    let mut inflight = InflightTurnState::new(
        provider.clone(),
        channel.get(),
        Some("rewind-owner-7219".into()),
        343_742_347_365_974_026,
        77_219,
        18,
        "prompt".into(),
        Some("session".into()),
        Some("rewind-owner-7219-session".into()),
        Some("/tmp/rewind-owner-7219.jsonl".into()),
        None,
        512,
    );
    inflight.turn_nonce = Some("episode-a".into());
    inflight.full_response = "this turn's delivered answer".into();
    inflight.response_sent_offset = inflight.full_response.len();
    let mut successor = inflight.clone();
    successor.turn_nonce = Some("episode-b".into());
    successor.full_response = "the successor's own answer".into();
    successor.response_sent_offset = successor.full_response.len();
    save_inflight_state(&successor).expect("seed the successor row");
    assert_eq!(
        InflightTurnIdentity::from_state(&successor),
        InflightTurnIdentity::from_state(&inflight)
    );
    let path = crate::services::discord::inflight::inflight_state_path(
        &crate::services::discord::inflight::inflight_runtime_root().unwrap(),
        &provider,
        channel.get(),
    );
    let successor_bytes = std::fs::read(&path).unwrap();
    let (mut sent, mut confirmed) = (inflight.response_sent_offset, 0usize);

    sync_terminal_error_delivery_state_for_bridge_owner(
        "provider failed",
        &mut sent,
        &mut confirmed,
        &mut inflight,
        channel,
        false,
    );

    assert_eq!(inflight.turn_nonce.as_deref(), Some("episode-a"));
    assert_ne!(inflight.full_response, successor.full_response);
    assert_eq!(std::fs::read(&path).unwrap(), successor_bytes);
}
