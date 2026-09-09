use super::finalize_epilogue::resume_pinned_watcher;
use super::*;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};

fn handle() -> TmuxWatcherHandle {
    TmuxWatcherHandle {
        tmux_session_name: "C1-source".into(),
        output_path: "/C1-source.jsonl".into(),
        paused: Arc::new(AtomicBool::new(true)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        pause_epoch: Arc::new(AtomicU64::new(0)),
        turn_delivered: Arc::new(AtomicBool::new(true)),
        last_heartbeat_ts_ms: Arc::new(AtomicI64::new(super::super::tmux_watcher_now_ms())),
    }
}
fn copy_handle(h: &TmuxWatcherHandle) -> TmuxWatcherHandle {
    TmuxWatcherHandle {
        tmux_session_name: h.tmux_session_name.clone(),
        output_path: h.output_path.clone(),
        paused: h.paused.clone(),
        resume_offset: h.resume_offset.clone(),
        cancel: h.cancel.clone(),
        pause_epoch: h.pause_epoch.clone(),
        turn_delivered: h.turn_delivered.clone(),
        last_heartbeat_ts_ms: h.last_heartbeat_ts_ms.clone(),
    }
}
fn unchanged(h: &TmuxWatcherHandle) {
    assert_eq!(*h.resume_offset.lock().unwrap(), None);
    assert!(h.paused.load(Ordering::Acquire));
    assert!(h.turn_delivered.load(Ordering::Acquire));
}
fn capture(shared: &SharedData, path: &str) -> Option<WatcherClaimIncarnation> {
    WatcherClaimIncarnation::capture_for_source(
        &shared.tmux_watchers,
        "C1-source",
        std::path::Path::new(path),
    )
}

#[test]
fn c1_missing_and_idle_tail_mismatch_never_resume_latest() {
    let shared = super::super::make_shared_data_for_tests();
    assert!(capture(&shared, "/C1-source.jsonl").is_none());
    let h = handle();
    shared
        .tmux_watchers
        .insert(ChannelId::new(580801), copy_handle(&h));
    // Process/no-handoff and Claude idle-tail path mismatch both lack a source pin.
    for pin in [None, capture(&shared, "/different-transcript.jsonl")] {
        assert!(pin.is_none());
        assert!(!resume_pinned_watcher(
            &shared.tmux_watchers,
            pin.as_ref(),
            900
        ));
        unchanged(&h);
    }
    assert!(
        WatcherClaimIncarnation::capture_for_source(
            &shared.tmux_watchers,
            "other-tmux",
            std::path::Path::new("/C1-source.jsonl")
        )
        .is_none()
    );
}

#[test]
fn c1_synthetic_and_handoff_stale_pins_leave_replacement_untouched() {
    let shared = super::super::make_shared_data_for_tests();
    let owner = ChannelId::new(580802);
    let a = handle();
    shared.tmux_watchers.insert(owner, copy_handle(&a));
    let synthetic = capture(&shared, "/C1-source.jsonl").unwrap();
    let mut handoff = None;
    super::runtime_handoff_loop::adopt_claimed_watcher_delivery_marker(&mut handoff, &synthetic);
    let b = handle();
    shared.tmux_watchers.insert(owner, copy_handle(&b));
    for pin in [Some(synthetic), handoff] {
        assert!(!resume_pinned_watcher(
            &shared.tmux_watchers,
            pin.as_ref(),
            901
        ));
        unchanged(&b);
        unchanged(&a);
    }
}

#[test]
fn c1_cancelled_registered_pin_leaves_all_effects_untouched() {
    let shared = super::super::make_shared_data_for_tests();
    let h = handle();
    shared
        .tmux_watchers
        .insert(ChannelId::new(580803), copy_handle(&h));
    let pin = capture(&shared, "/C1-source.jsonl").unwrap();
    h.cancel.store(true, Ordering::Release);
    assert!(!resume_pinned_watcher(
        &shared.tmux_watchers,
        Some(&pin),
        902
    ));
    assert!(capture(&shared, "/C1-source.jsonl").is_none());
    unchanged(&h);
}

#[test]
fn c1_same_synthetic_and_handoff_pin_resume_without_clearing_marker() {
    let shared = super::super::make_shared_data_for_tests();
    let h = handle();
    shared
        .tmux_watchers
        .insert(ChannelId::new(580804), copy_handle(&h));
    let pin = capture(&shared, "/C1-source.jsonl").unwrap();
    let mut handoff = None;
    super::runtime_handoff_loop::adopt_claimed_watcher_delivery_marker(&mut handoff, &pin);
    for (pin, offset) in [(Some(pin), 903), (handoff, 904)] {
        h.paused.store(true, Ordering::Release);
        assert!(resume_pinned_watcher(
            &shared.tmux_watchers,
            pin.as_ref(),
            offset
        ));
        assert_eq!(*h.resume_offset.lock().unwrap(), Some(offset));
        assert!(!h.paused.load(Ordering::Acquire));
        assert!(h.turn_delivered.load(Ordering::Acquire));
    }
}

#[test]
fn c1_poisoned_resume_lock_does_not_unpause() {
    let shared = super::super::make_shared_data_for_tests();
    let h = handle();
    shared
        .tmux_watchers
        .insert(ChannelId::new(580805), copy_handle(&h));
    let pin = capture(&shared, "/C1-source.jsonl").unwrap();
    let _ = std::panic::catch_unwind(|| {
        let _guard = h.resume_offset.lock().unwrap();
        panic!("poison resume mutex");
    });
    assert!(!resume_pinned_watcher(
        &shared.tmux_watchers,
        Some(&pin),
        905
    ));
    assert!(h.paused.load(Ordering::Acquire));
    assert_eq!(*h.resume_offset.lock().unwrap_err().into_inner(), None);
    assert!(h.turn_delivered.load(Ordering::Acquire));
}

#[test]
fn c1_both_late_writers_consume_pin_without_registry_backfill() {
    let entry = include_str!("mod.rs");
    assert!(entry.contains("CompletionPostludeState {\n                watcher_delivery_pin,"));
    assert!(entry.contains("TerminalOutcomeDeliveryContext {\n                    watcher_delivery_pin: watcher_delivery_pin.clone(),"));
    let completion = include_str!("completion_postlude.rs");
    let epilogue = include_str!("finalize_epilogue.rs");
    assert_eq!(
        completion
            .matches("finalize_epilogue::resume_pinned_watcher(")
            .count(),
        1
    );
    assert_eq!(
        epilogue
            .matches("resume_pinned_watcher(\n                    &shared_owned.tmux_watchers,")
            .count(),
        1
    );
    for source in [completion, epilogue] {
        assert!(!source.contains("tmux_watchers.get(&watcher_owner_channel_id)"));
        assert!(source.contains("watcher_delivery_pin.as_ref()"));
    }
}

#[test]
fn c1_actual_postlude_resume_pin_runtime_proof() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let shared = super::super::make_shared_data_for_tests();
        let owner = ChannelId::new(580899);
        let h = handle();
        shared.tmux_watchers.insert(owner, copy_handle(&h));
        let pin = capture(&shared, "/C1-source.jsonl").unwrap();
        let _mailbox = shared.mailbox(owner);
        let durable = InflightTurnState::new(ProviderKind::Codex, owner.get(), None, 1, 2, 0, String::new(), None, None, None, None, 0);
        let gateway: Arc<dyn TurnGateway> = Arc::new(super::super::gateway::HeadlessGateway);
        let mut bridge = TurnBridgeContext { provider: ProviderKind::Codex,
gateway: gateway.clone(),
channel_id: owner,
user_msg_id: None,
user_text_owned: String::new(),
request_owner_name: String::new(),
role_binding: None,
adk_session_key: None,
adk_session_name: None,
adk_session_info: None,
adk_cwd: None,
dispatch_id: None,
dispatch_kind: None,
memory_recall_usage: TokenUsage::default(),
context_window_tokens: 0,
context_compact_percent: 0,
current_msg_id: None,
response_sent_offset: 0,
full_response: String::new(),
tmux_last_offset: None,
new_session_id: None,
defer_watcher_resume: false,
reuse_status_panel_message: false,
completion_tx: None,
is_external_input_tui_direct: false,
inflight_state: durable.clone(), };
        let (completion_guard, mut inflight_guard) = super::guards::make_bridge_guards(&mut bridge, &durable, &shared, &ProviderKind::Codex);
        inflight_guard.defuse();
        let ctx = super::completion_postlude::CompletionPostludeContext { shared_owned: shared.clone(),
gateway: gateway.clone(),
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
turn_start: std::time::Instant::now(), };
        let state = super::completion_postlude::CompletionPostludeState { watcher_delivery_pin: Some(pin),
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
transport_error: false,
api_friction_reports: Vec::new(),
cancelled: false,
restart_followup_pending: None,
bridge_skip_holder_owns_inflight: false,
completion_guard: completion_guard,
inflight_guard: inflight_guard,
inflight_state: durable.clone(), };
        let future = super::completion_postlude::run_completion_postlude(ctx, state);
        tokio::pin!(future);
        // Poll the real caller through its resume effect; stop before unrelated downstream work.
        tokio::select! {
            _ = &mut future => {},
            _ = async { while h.resume_offset.lock().unwrap().is_none() { tokio::task::yield_now().await; } } => {},
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {},
        }
        assert_eq!(*h.resume_offset.lock().unwrap(), Some(580899), "actual completion caller must carry source pin");
        assert!(!h.paused.load(Ordering::Acquire));
        assert!(h.turn_delivered.load(Ordering::Acquire));
    });
}
