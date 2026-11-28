use super::finalize_epilogue::resume_pinned_watcher;
use super::*;

#[cfg(test)]
pub(in crate::services::discord) fn resume_incident_watcher(
    shared: &Arc<SharedData>,
    row: &crate::services::discord::inflight::InflightTurnState,
    pin: &WatcherClaimIncarnation,
) -> bool {
    // Seed the bridge handoff's ledger edges before unpausing the watcher.
    // The held intake fixture ends before bridge guards are constructed.
    let key = crate::services::discord::turn_finalizer::TurnKey::new(
        ChannelId::new(row.channel_id),
        row.effective_finalizer_turn_id(),
        shared.restart.current_generation,
    )
    .with_episode_nonce(row.turn_nonce.as_deref());
    shared.turn_finalizer.register_start_with_completion_admission(
        key,
        ProviderKind::Claude,
        crate::services::discord::inflight::RelayOwnerKind::Watcher,
        crate::services::discord::turn_finalizer::CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
        shared,
    );
    shared
        .turn_finalizer
        .note_terminal_disposition_settled(key, true, shared.clone());
    resume_pinned_watcher(&shared.tmux_watchers, Some(pin), 0)
}

// Per-channel rendezvous: only the real bridge capture site calls this hook.
type BridgeCaptureRendezvous = (
    ChannelId,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
pub(in crate::services::discord) static BRIDGE_CAPTURE_PROBE: std::sync::Mutex<
    Option<BridgeCaptureRendezvous>,
> = std::sync::Mutex::new(None);
pub(super) async fn after_bridge_capture(channel: ChannelId) {
    let probe = {
        let mut slot = BRIDGE_CAPTURE_PROBE.lock().unwrap();
        if slot.as_ref().is_some_and(|p| p.0 == channel) {
            slot.take()
        } else {
            None
        }
    };
    if let Some((_, captured, resume)) = probe {
        let _ = captured.send(());
        resume.await.unwrap();
    }
}

pub(in crate::services::discord) static BRIDGE_COMPLETION_PROBE: std::sync::Mutex<
    Option<(ChannelId, tokio::sync::oneshot::Sender<()>)>,
> = std::sync::Mutex::new(None);
pub(super) fn observe_bridge_completion(channel: ChannelId, task: tokio::task::JoinHandle<()>) {
    let probe = {
        let mut slot = BRIDGE_COMPLETION_PROBE.lock().unwrap();
        if slot.as_ref().is_some_and(|probe| probe.0 == channel) {
            slot.take()
        } else {
            None
        }
    };
    if let Some((_, completed)) = probe {
        tokio::spawn(async move {
            task.await.unwrap();
            let _ = completed.send(());
        });
    }
}

pub(in crate::services::discord) static WRONG_CAPTURE_CHANNEL: std::sync::Mutex<Option<ChannelId>> =
    std::sync::Mutex::new(None);
pub(super) fn capture_channel(channel: ChannelId) -> ChannelId {
    let mut wrong = WRONG_CAPTURE_CHANNEL.lock().unwrap();
    if *wrong == Some(channel) {
        *wrong = None;
        ChannelId::new(channel.get() + 1)
    } else {
        channel
    }
}

pub(in crate::services::discord) fn completion_postlude_fixture(
    bridge: &mut TurnBridgeContext,
    durable: InflightTurnState,
    shared: &Arc<SharedData>,
    pin: WatcherClaimIncarnation,
    clear_fence: tokio::sync::OnceCell<ChannelClearFence>,
    completion: Option<(&str, String, &str)>,
) -> impl std::future::Future<Output = ()> + use<> {
    let owner = bridge.channel_id;
    let (completion_guard, mut inflight_guard) =
        guards::make_bridge_guards(bridge, &durable, shared, &ProviderKind::Codex);
    inflight_guard.defuse();
    let mut ctx = completion_postlude::CompletionPostludeContext {
        shared_owned: shared.clone(),
        gateway: bridge.gateway.clone(),
        channel_id: owner,
        provider: ProviderKind::Codex,
        cancel_token: Arc::new(CancelToken::new()),
        user_msg_id: None,
        turn_id: "c1-runtime-postlude".into(),
        request_owner_name: String::new(),
        final_session_status: "idle",
        status_panel_started_at: 0,
        has_queued_turns: false,
        defer_watcher_resume: false,
        can_chain_locally: false,
        single_message_panel_footer_mode: false,
        is_external_input_tui_direct: false,
        context_window_tokens: 0,
        context_compact_percent: 0,
        clear_fence: clear_fence.into_inner().expect("bridge captured fence"),
        turn_start: std::time::Instant::now(),
    };
    let mut state = completion_postlude::CompletionPostludeState {
        watcher_delivery_pin: Some(pin),
        full_response: String::new(),
        user_text_owned: String::new(),
        role_binding: None,
        adk_session_key: None,
        adk_session_name: None,
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        new_session_id: None,
        new_raw_provider_session_id: None,
        status_panel_terminal_committed: false,
        bridge_should_emit_completion: false,
        current_msg_id: MessageId::new(580898),
        status_panel_msg_id: None,
        last_status_panel_text: String::new(),
        completion_footer_terminal_text: None,
        busy_requeue_outcome: None,
        spin_idx: 0,
        status_panel_generation: 0,
        preserve_inflight_for_cleanup_retry: true,
        tmux_last_offset: Some(580899),
        watcher_owner_channel_id: owner,
        bridge_relay_delegated_to_watcher: false,
        is_prompt_too_long: false,
        resume_failure_detected: false,
        auto_retry: Default::default(),
        recovery_retry: false,
        rx_disconnected: false,
        tmux_handed_off: false,
        bridge_output_owner: None,
        terminal_delivery_committed: false,
        terminal_session_reset_required: false,
        transcript_events: Vec::new(),
        accumulated_input_tokens: 0,
        accumulated_cache_create_tokens: 0,
        accumulated_cache_read_tokens: 0,
        accumulated_output_tokens: 0,
        accumulated_memory_input_tokens: 0,
        accumulated_memory_output_tokens: 0,
        input_not_submitted: false,
        transport_error: false,
        provider_failure: false,
        api_friction_reports: Vec::new(),
        cancelled: false,
        restart_followup_pending: None,
        bridge_skip_holder_owns_inflight: false,
        completion_guard,
        inflight_guard,
        inflight_state: durable.clone(),
    };
    if let Some((response, turn_id, case)) = completion {
        ctx.turn_id = turn_id;
        ctx.user_msg_id = Some(MessageId::new(9001)); // Predates the routine's own clear.
        state.adk_session_key = Some("isolated-routine-attempt".into());
        state.new_session_id = Some("routine-provider-session".into());
        state.full_response = response.into();
        state.user_text_owned = "브리핑".into();
        (
            state.terminal_delivery_committed,
            state.preserve_inflight_for_cleanup_retry,
            state.cancelled,
        ) = (true, false, case == "cancelled");
        if case == "token_cancelled" {
            ctx.cancel_token.cancelled.store(true, Ordering::Release);
        }
    }
    completion_postlude::run_completion_postlude(ctx, state)
}
