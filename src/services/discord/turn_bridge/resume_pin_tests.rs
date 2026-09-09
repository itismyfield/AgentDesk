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
fn c1_bridge_clear_fence_capture_reaches_the_postlude_context() {
    let entry = include_str!("mod.rs");
    // One producer only: a second capture at the delivery site would hand the
    // postlude a fence the bridge never observed before provider work started.
    assert_eq!(entry.matches("capture_channel_clear_fence(").count(), 1);
    let capture = entry
        .find("let clear_fence = capture_bridge_clear_fence(shared_owned.as_ref(), channel_id).await;")
        .expect("bridge captures its own clear fence");
    let deliver = entry
        .find("\n                clear_fence,\n                turn_start,\n")
        .expect("postlude context receives the captured fence verbatim");
    assert!(capture < deliver, "capture must precede postlude delivery");
}

#[test]
fn c1_actual_postlude_resume_pin_runtime_proof() {
    actual_postlude_runtime_proof(None, "own");
}

#[test]
fn sa2_actual_postlude_to_executor_pg() {
    for response in ["오늘 브리핑입니다", "NO_REPLY"] {
        actual_postlude_runtime_proof(Some(response), "own");
    }
}

#[test]
fn sa2_actual_postlude_rejects_cancelled_pg() {
    for case in ["cancelled", "token_cancelled"] {
        actual_postlude_runtime_proof(Some("NO_REPLY"), case);
    }
}

#[test]
fn sa2_actual_postlude_rejects_foreign_or_stale_pg() {
    for case in ["clear", "failed_capture", "foreign_attempt", "replacement"] {
        actual_postlude_runtime_proof(Some("NO_REPLY"), case);
    }
}

fn actual_postlude_runtime_proof(response: Option<&str>, case: &str) {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        use crate::db::session_transcripts::record_channel_clear_boundary;
        use crate::services::routines::{NewRoutine, RoutineAgentExecutor, RoutineStore};
        let db = if response.is_some() { Some(crate::dispatch::test_support::DispatchPostgresTestDb::create("sa2_postlude", "SA2 actual caller").await) } else { None };
        let pool = if let Some(db) = &db { Some(db.connect_and_migrate_with_max_connections(4).await) } else { None };
        let shared = super::super::make_shared_data_for_tests_with_storage(pool.clone());
        let owner = ChannelId::new(580899);
        let channel = owner.get().to_string();
        let turn_id = format!("discord:{channel}:9001");
        let (store, run_id) = if let Some(pool) = &pool {
            let store = RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 1024);
            let routine = store.attach_routine(NewRoutine { agent_id: None, fallback_agent_id: None, max_retries: None, script_ref: "fixture".into(), name: "fixture".into(), status: None, execution_strategy: "fresh".into(), schedule: None, next_due_at: None, checkpoint: None, discord_thread_id: None, timeout_secs: None }).await.unwrap();
            let run = store.claim_run_now(&routine.id).await.unwrap().unwrap();
            let thread = if case == "foreign_attempt" { "sibling" } else { &channel };
            // `replacement`: the in-flight run already moved on to a later turn id.
            let started = if case == "replacement" { format!("{turn_id}:replacement") } else { turn_id.clone() };
            assert!(store.mark_agent_turn_started(&run.run_id, &started, Some(serde_json::json!({"channel_id": channel, "discord_thread_id": thread})), "fixture", "fresh").await.unwrap());
            record_channel_clear_boundary(Some(pool), &channel).await.unwrap();
            super::super::session_runtime::rebind_channel_session(&shared, &ProviderKind::Codex, owner, root.path().to_str().unwrap(), "live-session").await;
            (Some(store), run.run_id)
        } else { (None, String::new()) };
        // `failed_capture`: the bridge observed nothing, so it carries the -1 sentinel.
        let observer = if case == "failed_capture" { super::super::make_shared_data_for_tests_with_storage(None) } else { shared.clone() };
        let clear_fence = super::capture_bridge_clear_fence(&observer, owner).await;
        if case == "clear" { record_channel_clear_boundary(pool.as_ref(), &channel).await.unwrap(); }
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
        let mut ctx = super::completion_postlude::CompletionPostludeContext { shared_owned: shared.clone(),
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
clear_fence,
turn_start: std::time::Instant::now(), };
        let mut state = super::completion_postlude::CompletionPostludeState { watcher_delivery_pin: Some(pin),
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
        if let Some(response) = response {
            ctx.turn_id = turn_id;
            ctx.user_msg_id = Some(MessageId::new(9001)); // Predates the routine's own clear.
            state.adk_session_key = Some("isolated-routine-attempt".into());
            state.new_session_id = Some("routine-provider-session".into());
            state.full_response = response.into();
            state.user_text_owned = "브리핑".into();
            (state.terminal_delivery_committed, state.preserve_inflight_for_cleanup_retry, state.cancelled) = (true, false, case == "cancelled");
            if case == "token_cancelled" { ctx.cancel_token.cancelled.store(true, Ordering::Release); }
            // A foreign mailbox/watcher must retain its live channel state even on success.
            assert!(_mailbox.try_start_turn(Arc::new(CancelToken::new()), UserId::new(2), MessageId::new(3)).await);
            tokio::time::timeout(std::time::Duration::from_secs(10), super::completion_postlude::run_completion_postlude(ctx, state)).await.unwrap();
            let session = shared.core.lock().await.sessions.get(&owner).unwrap().clone();
            assert_eq!((session.session_id.as_deref(), session.history.len()), (Some("live-session"), 0));
            unchanged(&h);
            assert_eq!(_mailbox.snapshot().await.active_user_message_id, Some(MessageId::new(3)));
            let pool = pool.as_ref().unwrap();
            let pairs = crate::db::session_transcripts::fetch_recent_channel_pairs(pool, &channel, 10).await.unwrap();
            assert_eq!(pairs.iter().map(|pair| &*pair.assistant_message).collect::<Vec<_>>(), if case == "own" { vec![response] } else { vec![] }, "case={case}");
            let outcomes = RoutineAgentExecutor::new(Arc::new(pool.clone()), None, 1800).poll_agent_runs(store.as_ref().unwrap(), 10, false).await.unwrap();
            assert_eq!(outcomes.len(), usize::from(case == "own"), "case={case}");
            if case == "own" { assert_eq!((&*outcomes[0].run_id, &*outcomes[0].status), (&*run_id, "succeeded")); }
            drop((shared, store));
            pool.close().await;
            db.unwrap().drop().await;
            return;
        }
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
