//! Start before the post-loop controller, not at the later receipt gate.

use super::*;
use crate::services::discord::{
    abandon_request_store,
    formatting::{LongRunningCloseTrigger, MonitorHandoffReason},
    placeholder_controller::{PlaceholderActiveInput, PlaceholderKey},
    turn_bridge::{guards::InflightCleanupGuard, post_loop_finalize::*},
};

async fn open_card(
    driver: &TerminalDeliveryDriver,
    ctx: &TerminalOutcomeDeliveryContext,
    state: &mut TerminalOutcomeDeliveryState,
) {
    let key = PlaceholderKey {
        provider: state.provider.clone(),
        channel_id: ctx.channel_id,
        message_id: ctx.current_msg_id,
    };
    let input = PlaceholderActiveInput {
        reason: MonitorHandoffReason::ExplicitCall,
        started_at_unix: 1_700_000_000,
        tool_summary: Some("Monitor".into()),
        command_summary: None,
        reason_detail: None,
        context_line: None,
        request_line: None,
        progress_line: None,
    };
    driver
        .shared
        .ui
        .placeholder_controller
        .ensure_active(driver.gateway.as_ref(), key.clone(), input.clone())
        .await;
    state.inflight_state.long_running_placeholder_active = true;
    state.long_running_placeholder_active = Some((
        key,
        input,
        LongRunningCloseTrigger::BackgroundDispatch,
        false,
    ));
}

async fn run_from_postloop(
    ctx: TerminalOutcomeDeliveryContext,
    state: TerminalOutcomeDeliveryState,
) -> TerminalOutcomeDeliveryOutput {
    run_from_postloop_with_source_advance(ctx, state, false).await
}

async fn run_from_postloop_with_source_advance(
    mut ctx: TerminalOutcomeDeliveryContext,
    mut state: TerminalOutcomeDeliveryState,
    advance_source: bool,
) -> TerminalOutcomeDeliveryOutput {
    let output = run_post_loop_finalize(
        PostLoopFinalizeContext {
            shared_owned: state.shared_owned.clone(),
            gateway: state.gateway.clone(),
            channel_id: ctx.channel_id,
            provider: state.provider.clone(),
            adk_session_key: state.adk_session_key.clone(),
            adk_session_name: None,
            adk_session_info: None,
            adk_cwd: None,
            dispatch_id: None,
            role_binding: None,
            turn_id: state.turn_id.clone(),
            current_msg_id: ctx.current_msg_id,
            entry_was_rowless: ctx.entry_was_rowless,
            synthetic_actor: None,
            codex_tui_terminal_range: ctx.codex_tui_terminal_range.clone(),
            cancelled: ctx.cancelled,
            transport_error: ctx.transport_error,
            tui_error_classification: ctx.tui_error_classification,
            recovery_retry: ctx.recovery_retry,
            rx_disconnected: ctx.rx_disconnected,
            tmux_handed_off: false,
            standby_relay_owns_output: false,
            watcher_owns_assistant_relay: false,
            watcher_relay_available_for_turn: false,
            bridge_entry_watcher_owner_epoch_current: true,
            response_sent_offset: state.response_sent_offset,
            tmux_last_offset: ctx.tmux_last_offset,
            watcher_owner_channel_id: ctx.watcher_owner_channel_id,
            accumulated_input_tokens: 0,
            accumulated_cache_create_tokens: 0,
            accumulated_cache_read_tokens: 0,
            accumulated_output_tokens: 0,
        },
        PostLoopFinalizeState {
            full_response: state.full_response,
            active_background_child_session_ids: state.active_background_child_session_ids,
            pending_long_running_open_after_state_save: state
                .pending_long_running_open_after_state_save,
            pending_long_running_retarget_after_state_save: state
                .pending_long_running_retarget_after_state_save,
            long_running_placeholder_active: state.long_running_placeholder_active,
            current_tool_line: None,
            prev_tool_status: None,
            inflight_state: state.inflight_state,
            api_friction_reports: state.api_friction_reports,
        },
    )
    .await;
    if advance_source {
        let generation =
            crate::services::tmux_common::session_temp_path(DRIVER_TMUX_SESSION, "generation");
        filetime::set_file_mtime(
            &generation,
            filetime::FileTime::from_unix_time(1_700_552_101, 1),
        )
        .unwrap();
    }
    ctx.preloop_receipt_confirmed = output.preloop_receipt_confirmed;
    state.full_response = output.full_response;
    state.active_background_child_session_ids = output.active_background_child_session_ids;
    state.pending_long_running_open_after_state_save =
        output.pending_long_running_open_after_state_save;
    state.pending_long_running_retarget_after_state_save =
        output.pending_long_running_retarget_after_state_save;
    state.long_running_placeholder_active = output.long_running_placeholder_active;
    state.inflight_state = output.inflight_state;
    state.api_friction_reports = output.api_friction_reports;
    state.review_dispatch_warning = output.review_dispatch_warning;
    ctx.bridge_output_owner = output.bridge_output_owner;
    ctx.bridge_relay_delegated_to_watcher = output.bridge_relay_delegated_to_watcher;
    ctx.should_complete_work_dispatch_after_delivery =
        output.should_complete_work_dispatch_after_delivery;
    ctx.should_fail_dispatch_after_delivery = output.should_fail_dispatch_after_delivery;
    ctx.can_chain_locally = output.can_chain_locally;
    ctx.is_prompt_too_long = output.is_prompt_too_long;
    ctx.claude_tui_followup_pre_submit_requeue_candidate =
        output.claude_tui_followup_pre_submit_requeue_candidate;
    ctx.tui_error_classification = output.tui_error_classification;
    #[cfg(unix)]
    {
        ctx.bridge_tui_gate_outcome_early = output.bridge_tui_gate_outcome_early;
    }
    run(ctx, state).await
}

#[tokio::test]
async fn receipted_rowless_turn_never_patches_active_card_before_terminal_gate_5521() {
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (ctx, mut state, source) = receipt_parts(&driver, provider);
        open_card(&driver, &ctx, &mut state).await;
        // A different actor publishes and removes the row after the bridge
        // opened its card, before the real post-loop phase starts.
        driver
            .gateway
            .replace_message_with_outcome(ctx.channel_id, ctx.current_msg_id, &state.full_response)
            .await
            .unwrap();
        dr::record_current_pinned_delivery(&source, ctx.current_msg_id.get()).unwrap();
        let before = driver.observations().len();
        let output = tokio::time::timeout(DRIVER_TIMEOUT, run_from_postloop(ctx, state))
            .await
            .expect("post-loop and terminal delivery must settle");
        assert!(output.terminal_delivery_committed);
        assert_eq!(
            driver.observations().len(),
            before,
            "an exact receipt must dominate even the pre-terminal controller PATCH"
        );
    }
}

#[tokio::test]
async fn unreceipted_rowless_turn_keeps_active_card_and_terminal_delivery_5521() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let (ctx, mut state, _) = receipt_parts(&driver, ProviderKind::Claude);
    open_card(&driver, &ctx, &mut state).await;
    let before = driver.observations().len();
    let output = tokio::time::timeout(DRIVER_TIMEOUT, run_from_postloop(ctx, state))
        .await
        .expect("unreceipted output must remain deliverable");
    assert!(output.terminal_delivery_committed);
    assert!(
        driver.observations()[before..]
            .iter()
            .any(|o| o.call == DriverCall::Edit)
    );
    assert!(driver.completed_publications() > 0);
}

#[tokio::test]
async fn aborted_delivery_future_preserves_same_user_and_zero_id_successor_5071() {
    for user_msg_id in [DRIVER_USER_MSG_ID, 0] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (mut ctx, mut state) = driver.parts();
        ctx.user_msg_id = None;
        state.inflight_state.user_msg_id = user_msg_id;
        state.inflight_state.turn_nonce = Some("captured-actor".into());
        inflight::save_inflight_state(&state.inflight_state).unwrap();
        let mut successor = state.inflight_state.clone();
        successor.turn_nonce = Some("successor-actor".into());
        successor.long_running_placeholder_active = true;
        let guard = InflightCleanupGuard::for_completion_test(
            &state.inflight_state,
            driver.shared.token_hash.clone(),
        );
        let mut future = Box::pin(async move {
            let _guard = guard;
            run_terminal_outcome_delivery(ctx, state).await
        });
        assert!(
            !poll_at_most(&mut future, 1),
            "stop inside real gateway await"
        );
        assert!(!driver.observations().is_empty());
        assert_eq!(driver.completed_publications(), 0);
        inflight::save_inflight_state(&successor).unwrap();
        let before = serde_json::to_value(
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, DRIVER_CHANNEL_ID)
                .unwrap(),
        )
        .unwrap();
        drop(future);
        let after = serde_json::to_value(
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, DRIVER_CHANNEL_ID)
                .expect("successor row survives old future"),
        )
        .unwrap();
        assert_eq!(after, before, "same user/id-0 is not the same episode");
        assert!(
            abandon_request_store::load_pending(&ProviderKind::Claude, &driver.shared.token_hash)
                .is_empty(),
            "old actor must not enqueue an abort PATCH for successor's card"
        );
        assert_eq!(driver.completed_publications(), 0);
    }
}

#[tokio::test]
async fn aborted_delivery_future_hands_off_its_own_placeholder_5071() {
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let (mut ctx, state) = driver.parts();
    ctx.user_msg_id = None;
    let guard = InflightCleanupGuard::for_completion_test(
        &state.inflight_state,
        driver.shared.token_hash.clone(),
    );
    let mut future = Box::pin(async move {
        let _guard = guard;
        run_terminal_outcome_delivery(ctx, state).await
    });
    assert!(!poll_at_most(&mut future, 1));
    drop(future);
    assert!(
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, DRIVER_CHANNEL_ID).is_none()
    );
    let requests =
        abandon_request_store::load_pending(&ProviderKind::Claude, &driver.shared.token_hash);
    assert_eq!(
        requests.len(),
        1,
        "own card is durably handed to the existing sweeper"
    );
}

#[tokio::test]
async fn preloop_preserves_foreign_same_anchor_card_with_or_without_old_receipt_5521() {
    for old_receipt in [false, true] {
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
        let (ctx, mut state, source) = receipt_parts(&driver, ProviderKind::Claude);
        open_card(&driver, &ctx, &mut state).await;
        let (key, mut successor_input, _, _) =
            state.long_running_placeholder_active.clone().unwrap();
        driver.shared.ui.placeholder_controller.detach(&key);
        successor_input.started_at_unix += 1;
        driver
            .shared
            .ui
            .placeholder_controller
            .ensure_active(
                driver.gateway.as_ref(),
                key.clone(),
                successor_input.clone(),
            )
            .await;
        let mut successor = state.inflight_state.clone();
        successor.turn_nonce = Some("successor-actor".into());
        inflight::save_inflight_state(&successor).unwrap();
        let before_row = serde_json::to_value(
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, DRIVER_CHANNEL_ID)
                .unwrap(),
        )
        .unwrap();
        if old_receipt {
            dr::record_current_pinned_delivery(&source, ctx.current_msg_id.get()).unwrap();
        }
        let before = driver.observations().len();
        tokio::time::timeout(DRIVER_TIMEOUT, run_from_postloop(ctx, state))
            .await
            .expect("foreign owner must not spin");
        assert_eq!(driver.observations().len(), before);
        let after_row = serde_json::to_value(
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, DRIVER_CHANNEL_ID)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(after_row, before_row);
        let outcome = driver
            .shared
            .ui
            .placeholder_controller
            .ensure_active(driver.gateway.as_ref(), key, successor_input)
            .await;
        assert!(matches!(outcome,
            crate::services::discord::placeholder_controller::PlaceholderControllerOutcome::Coalesced));
        assert_eq!(
            driver.observations().len(),
            before,
            "preloop must retain the successor controller incarnation"
        );
    }
}

#[test]
fn receipted_postloop_preserves_raw_proof_and_session_status_5521() {
    const CHILD: &str = "ADK_5521_RECEIPT_STATUS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Match the existing completion span fixture: API context is global,
        // so exercise the local webhook in a fresh process only.
        let root = tempfile::tempdir().unwrap();
        let name = format!(
            "{}::receipted_postloop_preserves_raw_proof_and_session_status_5521",
            module_path!().split_once("::").unwrap().1
        );
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env(CHILD, "1")
            .env("AGENTDESK_ROOT_DIR", root.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
        return;
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let calls = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let captured = calls.clone();
        let app = axum::Router::new().route("/api/dispatched-sessions/webhook", axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let captured = captured.clone();
            async move { captured.lock().unwrap().push(body); axum::Json(serde_json::json!({})) }
        }));
        let listener = tokio::net::TcpListener::bind((crate::config::loopback().as_str(), 0)).await.unwrap();
        crate::services::discord::internal_api::init(listener.local_addr().unwrap().port(), None);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for advance in [false, true] {
            let raw = format!("{DRIVER_BODY}\nAPI_FRICTION: {{\"endpoint\":\"/fixture\",\"friction_type\":\"missing_field\",\"summary\":\"fixture report\"}}");
            let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1).with_body(raw);
            let (ctx, mut state, source) = receipt_parts(&driver, ProviderKind::Codex);
            state.adk_session_key = Some("receipt-episode-A".into());
            let mut successor = state.inflight_state.clone();
            successor.turn_nonce = Some("successor-B".into());
            inflight::save_inflight_state(&successor).unwrap();
            let before = serde_json::to_value(inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID).unwrap()).unwrap();
            dr::record_current_pinned_delivery(&source, ctx.current_msg_id.get()).unwrap();
            let output = tokio::time::timeout(DRIVER_TIMEOUT, run_from_postloop_with_source_advance(ctx, state, advance)).await.unwrap();
            assert!(output.terminal_delivery_committed);
            assert!(!output.full_response.contains("API_FRICTION:"));
            assert_eq!(output.api_friction_reports.len(), 1);
            assert!(driver.observations().is_empty(), "raw exact receipt survives normalization and generation advancement");
            assert!(custody_records(&driver).is_empty());
            assert!(calls.lock().unwrap().is_empty(), "episode A must never write TURN_ACTIVE after its receipt or clobber B");
            assert_eq!(serde_json::to_value(inflight::load_inflight_state_read_only(&ProviderKind::Codex, DRIVER_CHANNEL_ID).unwrap()).unwrap(), before);
        }
        server.abort();
    });
}
