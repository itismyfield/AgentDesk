use super::super::provider_output_guard_tests::CapturingGateway;
use super::*;
use crate::services::discord::footer_retirement::legacy_for_test;
use crate::services::discord::inflight::{InflightTurnIdentity, save_inflight_state};
use crate::services::discord::placeholder_live_events::footer_activity_tests::{
    START, six_tool_fixture,
};
use crate::services::tui_o::cutover::{BodyClaim, test_override};

struct History {
    sends: Vec<String>,
    edits: Vec<String>,
    offset: usize,
    last_edit: String,
    state: InflightTurnState,
}

async fn drive(
    shared: Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    body: String,
    tool: Option<String>,
) -> History {
    let mut inflight = InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        0,
        18,
        "prompt".into(),
        None,
        None,
        None,
        None,
        0,
    );
    inflight.full_response = body.clone();
    save_inflight_state(&inflight).unwrap();
    let expected = InflightTurnIdentity::from_state(&inflight);
    let mut baseline = inflight.clone();
    let gateway = Arc::new(CapturingGateway {
        direct: true,
        ..Default::default()
    });
    let gateway_dyn: Arc<dyn TurnGateway> = gateway.clone();
    let (mut body, mut sent, mut confirmed, mut current) = (body, 0, 0, MessageId::new(18));
    let now = tokio::time::Instant::now();
    let (mut spin, mut panel_text, mut panel_edit, mut panel_dirty) =
        (0, String::new(), now, false);
    let (mut status_edit, mut last_edit, mut first_answer) = (
        now - std::time::Duration::from_secs(60),
        String::new(),
        false,
    );
    let (mut tool, mut prev_tool, mut tool_name, mut tool_summary) = (tool, None, None, None);
    let (mut tool_used, mut post_tool, mut frozen, mut tmux_offset) =
        (tool.is_some(), false, Vec::new(), None);
    let (
        mut watcher_channel,
        mut watcher_owns,
        mut watcher_available,
        mut watcher_pin,
        mut standby,
    ) = (channel, false, false, None, false);
    let (mut panel, mut generation, mut expected_message) = (None, 0, (18, 0));
    let (mut pending, mut created, mut pending_open, mut pending_retarget, mut active) =
        (None, None, None, None, None);
    let mut spans = crate::services::discord::turn_bridge::bridge_latency_spans::BridgeLatencySpans::starting_at(std::time::Instant::now());
    let mut dirty = false;
    let none = None;
    let actor_nonce = baseline.turn_nonce.clone();
    let context = runtime::TickContext {
        shared_owned: &shared,
        gateway: &gateway_dyn,
        channel_id: channel,
        provider,
        turn_id: "retired-raw-history",
        stream_tick_expected: &expected,
        captured_nonce: actor_nonce.as_deref(),
        status_interval: std::time::Duration::ZERO,
        single_message_panel_footer_mode: false,
        status_panel_started_at: START,
        done: false,
        dispatch_id: &none,
        adk_session_key: &none,
        spinner: &["⠋"],
    };
    let mut tick = runtime::TickRuntime {
        spin_idx: &mut spin,
        last_status_panel_text: &mut panel_text,
        last_status_panel_edit: &mut panel_edit,
        status_panel_dirty: &mut panel_dirty,
        full_response: &mut body,
        response_sent_offset: &mut sent,
        bridge_confirmed_response_sent_offset: &mut confirmed,
        current_msg_id: &mut current,
        current_tool_line: &mut tool,
        prev_tool_status: &mut prev_tool,
        last_tool_name: &mut tool_name,
        last_tool_summary: &mut tool_summary,
        any_tool_used: &mut tool_used,
        has_post_tool_text: &mut post_tool,
        streaming_rollover_frozen_msg_ids: &mut frozen,
        tmux_last_offset: &mut tmux_offset,
        watcher_owner_channel_id: &mut watcher_channel,
        watcher_owns_assistant_relay: &mut watcher_owns,
        watcher_relay_available_for_turn: &mut watcher_available,
        watcher_delivery_pin: &mut watcher_pin,
        standby_relay_owns_output: &mut standby,
        status_panel_msg_id: &mut panel,
        status_panel_generation: &mut generation,
        last_edit_text: &mut last_edit,
        last_status_edit: &mut status_edit,
        first_answer_relayed: &mut first_answer,
        pending_long_running_open_after_state_save: &mut pending_open,
        pending_long_running_retarget_after_state_save: &mut pending_retarget,
        long_running_placeholder_active: &mut active,
        inflight_state: &mut inflight,
        persisted_inflight_baseline: &mut baseline,
        expected_current_message: &mut expected_message,
        pending_current_message_candidate: &mut pending,
        bridge_created_response_placeholder_msg_id: &mut created,
        bridge_spans: &mut spans,
    };
    let claim = BodyClaim::new(channel.get(), None);
    let rolled = match run_rollover_loop(&context, &mut tick, &mut dirty, claim).await {
        ControlFlow::Continue(rolled) => rolled,
        ControlFlow::Break(()) => panic!("owned inflight row lost authority"),
    };
    assert_eq!(
        refresh_streaming_response(&context, &mut tick, &mut dirty, rolled, "⠋", claim).await,
        ControlFlow::Continue(())
    );
    assert!(dirty);
    let sends = gateway.sends.lock().unwrap().clone();
    let edits = gateway.edits.lock().unwrap().clone();
    History {
        sends,
        edits,
        offset: sent,
        last_edit,
        state: inflight,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn retired_bridge_rollover_raw_history_uses_minimal_anchor_with_legacy_budget() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let _legacy_channels = test_override::force_channels(&[]);
    for (index, provider) in [ProviderKind::Claude, ProviderKind::Codex]
        .into_iter()
        .enumerate()
    {
        for (unit_index, unit) in ["x", "한", "🙂"].into_iter().enumerate() {
            let channel = ChannelId::new(8_071_850 + index as u64 * 10 + unit_index as u64);
            let shared = six_tool_fixture(crate::config::ToolActivityDisplay::Summary, channel);
            let body = unit.repeat(1990 / unit.encode_utf16().count());
            let old_plan = {
                let _legacy = legacy_for_test();
                let budget = build_bridge_single_message_panel_status_block(
                    &shared, channel, &provider, START, "⠋", None, None, &body,
                );
                assert!(
                    budget.len()
                        > crate::services::discord::formatting::build_processing_status_block("⠋")
                            .len(),
                    "the oracle must use a rich legacy block"
                );
                crate::services::discord::formatting::plan_streaming_rollover(&body, &budget)
                    .unwrap()
            };
            let spinner_plan = crate::services::discord::formatting::plan_streaming_rollover(
                &body,
                &crate::services::discord::formatting::build_processing_status_block("⠋"),
            )
            .unwrap();
            assert_ne!(
                old_plan.split_at, spinner_plan.split_at,
                "fixture distinguishes rich budget from visible spinner"
            );
            assert!(old_plan.split_at < body.len() && old_plan.split_at > body.len() / 2);
            let history = drive(shared, &provider, channel, body.clone(), None).await;
            assert_eq!(
                history.offset, old_plan.split_at,
                "actual rollover must retain rich legacy budget"
            );
            assert_eq!(
                history.sends,
                ["⠋ 계속 처리 중"],
                "all raw POSTs must be minimal anchors"
            );
            assert_eq!(
                history.edits.first().unwrap().as_bytes(),
                old_plan.frozen_chunk.as_bytes()
            );
            assert_eq!(history.edits.last().unwrap(), &body[old_plan.split_at..]);
            assert_eq!(history.last_edit, body[old_plan.split_at..]);
            assert_eq!(history.state.response_sent_offset, old_plan.split_at);
            for text in history.sends.iter().chain(&history.edits) {
                assert!(
                    !text.contains("턴 시작")
                        && !text.contains("도구 6")
                        && !text.contains("fixture0"),
                    "raw write leaked rich budget: {text}"
                );
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn retired_bridge_empty_and_tool_only_raw_edits_use_minimal_anchor() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let _legacy_channels = test_override::force_channels(&[]);
    for (index, provider) in [ProviderKind::Claude, ProviderKind::Codex]
        .into_iter()
        .enumerate()
    {
        for (variant, (v2, tool)) in [
            (false, None),
            (true, None),
            (false, Some("⚙ Bash …")),
            (true, Some("⚙ Bash …")),
        ]
        .into_iter()
        .enumerate()
        {
            let channel = ChannelId::new(8_071_880 + index as u64 * 10 + variant as u64);
            let mut shared = six_tool_fixture(crate::config::ToolActivityDisplay::Summary, channel);
            Arc::get_mut(&mut shared)
                .unwrap()
                .ui
                .status_panel_v2_enabled = v2;
            let history = drive(
                shared,
                &provider,
                channel,
                String::new(),
                tool.map(str::to_owned),
            )
            .await;
            assert!(history.sends.is_empty());
            assert_eq!(
                history.edits,
                ["⠋ 계속 처리 중"],
                "empty and tool-only raw PATCHes must not display the rich budget"
            );
            assert_eq!(history.offset, 0);
            assert_eq!(history.last_edit, "⠋ 계속 처리 중");
            assert_eq!(history.state.current_msg_len, history.last_edit.len());
        }
    }
}
