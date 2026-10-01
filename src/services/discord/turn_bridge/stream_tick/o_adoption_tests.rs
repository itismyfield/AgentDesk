use super::provider_output_guard_tests::CapturingGateway;
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::channel_policy::{Adoption, BodyCheck};
use crate::services::tui_o::cutover::test_override;

const BODY: &str = "ADK-C1A-bridge-tick-body";

/// One real bridge stream tick for a Codex TUI turn whose anchor already exists.
async fn tick(channel: ChannelId, full_response: &str, gateway: Arc<CapturingGateway>, done: bool) {
    let shared = crate::services::discord::make_shared_data_for_tests();
    tick_with(shared, channel, full_response, gateway, done, false).await;
}

/// The same tick on `shared`, in footer mode or not; returns the turn's inflight state after it.
async fn tick_with(
    shared: Arc<SharedData>,
    channel: ChannelId,
    full_response: &str,
    gateway: Arc<CapturingGateway>,
    done: bool,
    footer: bool,
) -> InflightTurnState {
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
            single_message_panel_footer_mode: footer,
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
    inflight_state
}

/// A tick that streams no body (no unsent visible text, or a done tick that leaves the answer to
/// the terminal delivery) leaves a pending adoption; the tick that streams one ends it first.
#[tokio::test(flavor = "current_thread")]
async fn only_a_tick_that_streams_a_body_ends_a_pending_adoption() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let channel = ChannelId::new(42_593_310);
    let _candidates =
        test_override::force_candidates(&[(channel.get(), RuntimeHandoffKind::CodexTui)]);
    let check = BodyCheck::watch(channel.get(), BODY);
    let gateway = || {
        Arc::new(CapturingGateway {
            check: Some(check.clone()),
            ..Default::default()
        })
    };

    for (unsent, done) in [("", false), (" \n", false), (BODY, true)] {
        let quiet = gateway();
        tick(channel, unsent, quiet.clone(), done).await;
        assert!(
            quiet
                .edits
                .lock()
                .unwrap()
                .iter()
                .all(|edit| !edit.contains(BODY))
        );
        check.assert_settled();
        assert_eq!(check.adoption(), Adoption::Pending, "{unsent:?}");
    }

    let body = gateway();
    tick(channel, BODY, body.clone(), false).await;
    check.assert_settled();
    let edits = body.edits.lock().unwrap().clone();
    let shown: Vec<_> = edits.iter().filter(|edit| edit.contains(BODY)).collect();
    assert_eq!(shown.len(), 1, "{edits:?}");
}

/// A footer-mode shared state whose live panel saw `Bash` start in `channel`.
fn panel_with_last_tool(channel: ChannelId) -> Arc<SharedData> {
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .expect("fresh shared")
        .ui
        .status_panel_v2_enabled = true;
    let events = crate::services::discord::placeholder_live_events::status_events_from_tool_use(
        "Bash",
        r#"{"command":"cargo test"}"#,
    );
    shared
        .ui
        .placeholder_live_events
        .push_status_events(channel, events);
    shared
}

/// On O's channel the placeholder is the live panel: a status frame with the last tool and no
/// body, sent again below O's newest post with the old panel deleted once the new one is bound.
#[tokio::test(flavor = "current_thread")]
async fn o_channel_panel_shows_the_last_tool_and_moves_below_o_posts() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let (channel, moved) = (ChannelId::new(42_593_320), ChannelId::new(42_593_321));
    let _o = test_override::force_channels(&[
        (channel.get(), RuntimeHandoffKind::CodexTui),
        (moved.get(), RuntimeHandoffKind::CodexTui),
    ]);
    let shared = panel_with_last_tool(channel);

    let edit = Arc::new(CapturingGateway {
        direct: true,
        ..Default::default()
    });
    let state = tick_with(shared.clone(), channel, BODY, edit.clone(), false, true).await;
    let edits = edit.edits.lock().unwrap().clone();
    assert!(
        matches!(edits.as_slice(), [frame] if frame.contains("Bash") && !frame.contains(BODY)),
        "{edits:?}"
    );
    assert!(edit.sends.lock().unwrap().is_empty() && edit.deletes.lock().unwrap().is_empty());
    assert_eq!(state.current_msg_id, 18);

    crate::services::tui_o::writer::deliver::note_posted_for_tests(moved.get(), 50);
    let resend = Arc::new(CapturingGateway {
        send_id: 60,
        direct: true,
        ..Default::default()
    });
    let shared = panel_with_last_tool(moved);
    let state = tick_with(shared, moved, BODY, resend.clone(), false, true).await;
    let sends = resend.sends.lock().unwrap().clone();
    assert!(
        matches!(sends.as_slice(), [frame] if frame.contains("Bash") && !frame.contains(BODY)),
        "{sends:?}"
    );
    assert_eq!(*resend.deletes.lock().unwrap(), [18]);
    assert_eq!(state.current_msg_id, 60);
    let durable = crate::services::discord::inflight::load_inflight_state_read_only(
        &ProviderKind::Codex,
        moved.get(),
    )
    .expect("durable row");
    assert_eq!(
        durable.current_msg_id, 60,
        "the moved panel is the turn's bound placeholder"
    );
}

/// A channel O does not own never moves its placeholder below O's posts.
#[tokio::test(flavor = "current_thread")]
async fn legacy_channel_placeholder_is_never_moved() {
    let temp = tempfile::TempDir::new().expect("runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let channel = ChannelId::new(42_593_330);
    let _legacy = test_override::force_channels(&[]);
    crate::services::tui_o::writer::deliver::note_posted_for_tests(channel.get(), 50);
    let gateway = Arc::new(CapturingGateway {
        send_id: 60,
        direct: true,
        ..Default::default()
    });
    let shared = panel_with_last_tool(channel);
    let state = tick_with(shared, channel, "", gateway.clone(), false, true).await;
    assert!(gateway.sends.lock().unwrap().is_empty() && gateway.deletes.lock().unwrap().is_empty());
    assert_eq!(state.current_msg_id, 18);
}
