use super::provider_output_guard_tests::CapturingGateway;
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::channel_policy::Adoption;
use crate::services::tui_o::cutover::test_override;

const BODY: &str = "ADK-C1A-bridge-tick-body";

/// One real bridge stream tick for a Codex TUI turn whose anchor already exists.
async fn tick(channel: ChannelId, full_response: &str, gateway: Arc<CapturingGateway>) {
    let mut inflight_state = InflightTurnState::new(
        ProviderKind::Codex,
        channel.get(),
        Some("adk-c1a".to_string()),
        343_742_347_365_974_026,
        77_010,
        18,
        "prompt".to_string(),
        Some("session".to_string()),
        Some("AgentDesk-codex-c1a-tick".to_string()),
        Some("/tmp/AgentDesk-codex-c1a-tick.jsonl".to_string()),
        None,
        0,
    );
    inflight_state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
    crate::services::discord::inflight::save_inflight_state(&inflight_state).expect("seed row");
    let expected =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(&inflight_state);
    let mut baseline = inflight_state.clone();
    let mut expected_current_message = (18, 0);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let gateway: Arc<dyn TurnGateway> = gateway;
    let mut current_msg_id = crate::services::discord::turn_bridge::current_message_anchor::detached_current_msg_id_from_durable(18);
    let (mut full_response, mut sent, mut confirmed) = (full_response.to_string(), 0, 0);
    let now = tokio::time::Instant::now();
    let (mut dirty, mut panel_dirty, mut refresh, mut panel_edit, mut status_edit) = (
        false,
        false,
        now,
        now,
        now - std::time::Duration::from_secs(60),
    );
    let (mut spin_idx, mut panel_msg_id, mut panel_text) = (0usize, None, String::new());
    let (mut watcher_owns, mut watcher_available, mut pin, mut standby) =
        (false, false, None, false);
    let mut watcher_channel = ChannelId::new(1);
    let (mut frozen, mut candidate, mut created) = (Vec::new(), None, None);
    let (mut last_edit_text, mut first_answer_relayed) = (String::new(), false);
    let (mut tool_line, mut prev_tool, mut tool_name, mut tool_summary) = (None, None, None, None);
    let (mut any_tool_used, mut post_tool_text, mut tmux_offset) = (false, false, None);
    let mut spans = crate::services::discord::turn_bridge::bridge_latency_spans::BridgeLatencySpans::starting_at(
        std::time::Instant::now(),
    );
    let (mut generation, mut open_after, mut retarget_after, mut long_running) =
        (0u64, None, None, None);
    let (mut heartbeat, mut long_run_heartbeat) =
        (std::time::Instant::now(), std::time::Instant::now());
    let outcome = run_bridge_stream_tick(
        BridgeStreamTickContext {
            shared_owned: shared.clone(),
            gateway,
            channel_id: channel,
            provider: &ProviderKind::Codex,
            turn_id: "c1a-adoption-tick",
            expected_identity: &expected,
            status_interval: std::time::Duration::ZERO,
            single_message_panel_footer_mode: false,
            footer_owner:
                crate::services::discord::footer_view_reconciler::CompletionFooterOwner::new(
                    77_010, 0,
                ),
            status_panel_started_at: 0,
            done: false,
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
            status_panel_msg_id: &mut panel_msg_id,
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
            current_msg_id: &mut current_msg_id,
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
            inflight_state: &mut inflight_state,
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
    assert_eq!(outcome, StreamTickOutcome::Continue);
}

/// A tick with no unsent body leaves a pending adoption alone; the tick that streams a body ends it
/// before its edit, and Legacy shows that body once.
#[tokio::test(flavor = "current_thread")]
async fn only_a_tick_that_streams_a_body_ends_a_pending_adoption() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let channel = ChannelId::new(42_593_310);
    let _candidates =
        test_override::force_candidates(&[(channel.get(), RuntimeHandoffKind::CodexTui)]);
    let adoption =
        test_override::with_channels(|boot| boot.unwrap().candidate(channel.get()).cloned());
    let gateway = || {
        Arc::new(CapturingGateway {
            adoption: adoption.clone(),
            ..Default::default()
        })
    };

    let empty = gateway();
    tick(channel, "", empty.clone()).await;
    assert_eq!(adoption.as_ref().unwrap().peek(), Adoption::Pending);

    let body = gateway();
    tick(channel, BODY, body.clone()).await;
    assert_eq!(adoption.as_ref().unwrap().peek(), Adoption::Released);
    let edits = body.edits.lock().unwrap().clone();
    let shown: Vec<_> = edits.iter().filter(|edit| edit.contains(BODY)).collect();
    assert_eq!(shown.len(), 1, "{edits:?}");
    let seen = body.seen.lock().unwrap().clone();
    assert!(
        !seen.is_empty() && seen.iter().all(|state| *state == Adoption::Released),
        "{seen:?}"
    );
}
