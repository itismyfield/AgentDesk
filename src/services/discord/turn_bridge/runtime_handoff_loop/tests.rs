use super::*;
use crate::services::discord::inflight::{
    GuardedSaveOutcome, load_inflight_state, save_inflight_state,
};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};

fn runtime_seed(provider: ProviderKind, channel_id: u64) -> InflightTurnState {
    InflightTurnState::new(
        provider,
        channel_id,
        Some("adk-4259-r2".to_string()),
        343_742_347_365_974_026,
        77_010,
        18,
        "runtime handoff".to_string(),
        Some("provider-session-before-handoff".to_string()),
        None,
        Some("/seeded/runtime-output.jsonl".to_string()),
        None,
        512,
    )
}

fn live_watcher_handle(tmux_session_name: &str, output_path: &str) -> TmuxWatcherHandle {
    TmuxWatcherHandle {
        tmux_session_name: tmux_session_name.to_string(),
        output_path: output_path.to_string(),
        paused: Arc::new(AtomicBool::new(false)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        pause_epoch: Arc::new(AtomicU64::new(0)),
        turn_delivered: Arc::new(AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(AtomicI64::new(super::super::tmux_watcher_now_ms())),
    }
}

struct HandoffObservation {
    outcome: Option<GuardedSaveOutcome>,
    retry_message: Option<RuntimeHandoffLoopMessage>,
    terminal_control_ready_observed: bool,
    tmux_last_offset: Option<u64>,
    watcher_owner_channel_id: ChannelId,
    terminal_control_drain_until: Option<std::time::Instant>,
    claim_outcome: WatcherHandoffClaimOutcome,
    tmux_handed_off: bool,
    watcher_relay_available: bool,
    watcher_delivery_pin: Option<Arc<AtomicBool>>,
    watcher_slots: usize,
}

async fn dispatch_process_handoff_with_pin(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &mut InflightTurnState,
    message: RuntimeHandoffLoopMessage,
    state_dirty: &mut bool,
    done: bool,
    initial_watcher_delivery_pin: Option<Arc<AtomicBool>>,
) -> HandoffObservation {
    let channel_id = ChannelId::new(state.channel_id);
    let mut terminal_control_ready_observed = false;
    let mut tmux_last_offset = None;
    let mut watcher_owner_channel_id = channel_id;
    let mut standby_relay_owns_output = false;
    let mut watcher_relay_available_for_turn = false;
    let mut watcher_delivery_pin = initial_watcher_delivery_pin;
    let mut watcher_handoff_claim_outcome = WatcherHandoffClaimOutcome::None;
    let mut tmux_handed_off = false;
    let mut watcher_owns_assistant_relay = false;
    let mut terminal_control_drain_until = done.then(|| {
        std::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(60))
            .expect("terminal drain deadline")
    });
    let mut last_activity_heartbeat_at = None;

    let outcome = handle_runtime_handoff_loop_message(
        message,
        RuntimeHandoffLoopContext {
            shared_owned: shared,
            provider,
            channel_id,
            done,
            adk_session_name: &None,
        },
        RuntimeHandoffLoopState {
            terminal_control_ready_observed: &mut terminal_control_ready_observed,
            tmux_last_offset: &mut tmux_last_offset,
            inflight_state: state,
            watcher_owner_channel_id: &mut watcher_owner_channel_id,
            standby_relay_owns_output: &mut standby_relay_owns_output,
            watcher_relay_available_for_turn: &mut watcher_relay_available_for_turn,
            watcher_delivery_pin: &mut watcher_delivery_pin,
            watcher_handoff_claim_outcome: &mut watcher_handoff_claim_outcome,
            tmux_handed_off: &mut tmux_handed_off,
            watcher_owns_assistant_relay: &mut watcher_owns_assistant_relay,
            state_dirty,
            terminal_control_drain_until: &mut terminal_control_drain_until,
            last_activity_heartbeat_at: &mut last_activity_heartbeat_at,
        },
    )
    .await;
    HandoffObservation {
        outcome: outcome.guarded_save_outcome,
        retry_message: outcome.retry_message,
        terminal_control_ready_observed,
        tmux_last_offset,
        watcher_owner_channel_id,
        terminal_control_drain_until,
        claim_outcome: watcher_handoff_claim_outcome,
        tmux_handed_off,
        watcher_relay_available: watcher_relay_available_for_turn,
        watcher_delivery_pin,
        watcher_slots: shared.tmux_watchers.len(),
    }
}

async fn dispatch_process_handoff(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &mut InflightTurnState,
    message: RuntimeHandoffLoopMessage,
    state_dirty: &mut bool,
    done: bool,
) -> HandoffObservation {
    dispatch_process_handoff_with_pin(shared, provider, state, message, state_dirty, done, None)
        .await
}

#[tokio::test]
async fn process_runtime_ready_first_population_is_saved() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let provider = ProviderKind::Codex;
    let mut state = runtime_seed(provider.clone(), 42_592_601);
    save_inflight_state(&state).expect("seed owner row");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mut state_dirty = false;

    let outcome = dispatch_process_handoff(
        &shared,
        &provider,
        &mut state,
        RuntimeHandoffLoopMessage::RuntimeReady {
            handoff: RuntimeHandoff::ProcessBackend {
                output_path: "/runtime/process-ready.jsonl".to_string(),
                session_name: "process-session-r2".to_string(),
                last_offset: 4096,
            },
        },
        &mut state_dirty,
        false,
    )
    .await;

    assert_eq!(outcome.outcome, Some(GuardedSaveOutcome::Saved));
    assert!(outcome.retry_message.is_none());
    assert!(
        state_dirty,
        "the existing dirty flag from watcher-owner normalization remains queued"
    );
    let persisted = load_inflight_state(&provider, state.channel_id).expect("persisted row");
    assert_eq!(
        persisted.runtime_kind,
        Some(RuntimeHandoffKind::ProcessBackend)
    );
    assert_eq!(
        persisted.tmux_session_name.as_deref(),
        Some("process-session-r2")
    );
    assert_eq!(persisted.last_offset, 4096);
}

#[tokio::test]
async fn transient_runtime_stamp_io_error_restores_local_and_requeues_exact_handoff() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let temp = tempfile::tempdir().expect("runtime root parent");
    let blocked_root = temp.path().join("blocked-root");
    std::fs::write(&blocked_root, b"not a directory").expect("block runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        &blocked_root,
    );
    let provider = ProviderKind::Codex;
    let mut state = runtime_seed(provider.clone(), 42_592_604);
    let baseline = state.clone();
    let message = RuntimeHandoffLoopMessage::RuntimeReady {
        handoff: RuntimeHandoff::CodexTui {
            rollout_path: "/runtime/transient-retry.jsonl".to_string(),
            thread_id: Some("codex-thread-retry".to_string()),
            tmux_session_name: "AgentDesk-codex-runtime-retry".to_string(),
            last_offset: 8_192,
        },
    };
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mut state_dirty = false;

    let observed = dispatch_process_handoff(
        &shared,
        &provider,
        &mut state,
        message.clone(),
        &mut state_dirty,
        true,
    )
    .await;

    assert_eq!(observed.outcome, Some(GuardedSaveOutcome::IoError));
    assert_eq!(observed.retry_message, Some(message));
    assert!(!observed.terminal_control_ready_observed);
    assert_eq!(observed.tmux_last_offset, None);
    assert!(observed.watcher_delivery_pin.is_none());
    assert_eq!(
        observed.watcher_owner_channel_id,
        ChannelId::new(state.channel_id)
    );
    assert!(
        observed.terminal_control_drain_until.is_some(),
        "done=true retry must retain its residual lifecycle deadline",
    );
    assert!(
        !state_dirty,
        "the identity-mutated projection must not leak into generic dirty flush retry"
    );
    assert_eq!(
        serde_json::to_value(&state).unwrap(),
        serde_json::to_value(&baseline).unwrap(),
        "transient stamp failure must restore the exact pre-mutation local snapshot"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn second_watcher_owner_stamp_io_error_retries_from_exact_partial_checkpoint() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let provider = ProviderKind::Codex;
    let channel_id = 42_592_605;
    let incumbent_channel = ChannelId::new(channel_id + 100);
    let tmux_session_name = "AgentDesk-codex-r8-second-stamp";
    let output_path = "/runtime/r8-second-stamp.jsonl";
    let mut state = runtime_seed(provider.clone(), channel_id);
    save_inflight_state(&state).expect("seed pre-handoff row");
    state = load_inflight_state(&provider, channel_id).expect("load exact pre-handoff row");
    let pre_frame = state.clone();
    let message = RuntimeHandoffLoopMessage::RuntimeReady {
        handoff: RuntimeHandoff::CodexTui {
            rollout_path: output_path.to_string(),
            thread_id: Some("codex-thread-r8-second-stamp".to_string()),
            tmux_session_name: tmux_session_name.to_string(),
            last_offset: 8_192,
        },
    };
    let shared = crate::services::discord::make_shared_data_for_tests();
    let incumbent = live_watcher_handle(tmux_session_name, output_path);
    let incumbent_delivery_pin = Arc::clone(&incumbent.turn_delivered);
    shared.tmux_watchers.insert(incumbent_channel, incumbent);
    let pre_frame_delivery_pin = Arc::new(AtomicBool::new(false));
    let mut state_dirty = false;
    let _fail_second_stamp = guarded_save::fail_guarded_runtime_atomic_stamp_on_call(2);

    let failed = dispatch_process_handoff_with_pin(
        &shared,
        &provider,
        &mut state,
        message.clone(),
        &mut state_dirty,
        false,
        Some(Arc::clone(&pre_frame_delivery_pin)),
    )
    .await;

    assert_eq!(failed.outcome, Some(GuardedSaveOutcome::IoError));
    assert_eq!(failed.retry_message, Some(message.clone()));
    assert_eq!(failed.watcher_owner_channel_id, ChannelId::new(channel_id));
    assert_eq!(failed.claim_outcome, WatcherHandoffClaimOutcome::None);
    assert!(!failed.tmux_handed_off);
    assert!(!failed.watcher_relay_available);
    let failed_pin = failed
        .watcher_delivery_pin
        .as_ref()
        .expect("IoError restores the detached pre-frame pin");
    assert!(Arc::ptr_eq(failed_pin, &pre_frame_delivery_pin));
    assert!(!Arc::ptr_eq(failed_pin, &incumbent_delivery_pin));
    assert_eq!(failed.watcher_slots, 1);
    assert!(!state_dirty);
    assert_ne!(
        state.save_generation, pre_frame.save_generation,
        "the first watcher admission stamp must be the retained durable checkpoint",
    );
    let partial =
        load_inflight_state(&provider, channel_id).expect("load partial durable checkpoint");
    assert_eq!(
        serde_json::to_value(&state).unwrap(),
        serde_json::to_value(&partial).unwrap(),
        "IoError must retain the exact last committed row while detached owner rolls back",
    );

    let retried = dispatch_process_handoff_with_pin(
        &shared,
        &provider,
        &mut state,
        message,
        &mut state_dirty,
        false,
        failed.watcher_delivery_pin,
    )
    .await;
    assert_eq!(retried.outcome, Some(GuardedSaveOutcome::Saved));
    assert!(retried.retry_message.is_none());
    assert_eq!(retried.watcher_owner_channel_id, incumbent_channel);
    let retried_pin = retried
        .watcher_delivery_pin
        .as_ref()
        .expect("successful retry retains the first adopted pin");
    assert!(!Arc::ptr_eq(retried_pin, &pre_frame_delivery_pin));
    assert!(Arc::ptr_eq(retried_pin, &incumbent_delivery_pin));
    let durable = load_inflight_state(&provider, channel_id).expect("load retried durable row");
    assert_eq!(
        durable.watcher_owner_channel_id,
        Some(incumbent_channel.get())
    );
    assert_eq!(
        state.watcher_owner_channel_id,
        Some(incumbent_channel.get())
    );
    assert!(
        state_dirty,
        "post-stamp relay activation remains an ordinary guarded-flush delta",
    );
}

#[cfg(unix)]
#[rustfmt::skip]
async fn assert_claim_eviction_fails_closed(channel_id: u64, message: RuntimeHandoffLoopMessage) {
    let _lock = crate::config::shared_test_env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
    let provider = ProviderKind::Codex;
    let mut state = runtime_seed(provider.clone(), channel_id);
    save_inflight_state(&state).unwrap();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let (tmux, output) = match &message {
        RuntimeHandoffLoopMessage::TmuxReady { tmux_session_name, output_path, .. } => (tmux_session_name, output_path),
        RuntimeHandoffLoopMessage::RuntimeReady { handoff: RuntimeHandoff::CodexTui { tmux_session_name, rollout_path, .. } } => (tmux_session_name, rollout_path),
        _ => unreachable!(),
    };    shared.tmux_watchers.insert(ChannelId::new(channel_id), live_watcher_handle(tmux, output));
    let _eviction = crate::services::discord::tmux::evict_claim_before_adoption_for_test(ChannelId::new(channel_id));
    let mut dirty = false;
    let observed = dispatch_process_handoff(&shared, &provider, &mut state, message, &mut dirty, false).await;
    assert_eq!((observed.outcome, observed.claim_outcome), (Some(GuardedSaveOutcome::Saved), WatcherHandoffClaimOutcome::None));
    assert!(!observed.tmux_handed_off && !observed.watcher_relay_available && observed.watcher_delivery_pin.is_none());
    assert_eq!(state.effective_relay_owner_kind(), crate::services::discord::inflight::RelayOwnerKind::None);
}

#[cfg(unix)]
#[rustfmt::skip]
#[tokio::test(flavor = "current_thread")]
async fn runtime_adoption_rejects_claim_evicted_before_publication() {
    assert_claim_eviction_fails_closed(42_592_609, RuntimeHandoffLoopMessage::RuntimeReady { handoff: RuntimeHandoff::CodexTui { rollout_path: "/runtime/claim-evicted.jsonl".into(), thread_id: None, tmux_session_name: "AgentDesk-codex-claim-evicted".into(), last_offset: 10_240 } }).await;
}

#[cfg(unix)]
#[rustfmt::skip]
#[tokio::test(flavor = "current_thread")]
async fn direct_tmux_adoption_rejects_claim_evicted_before_publication() {
    assert_claim_eviction_fails_closed(42_592_610, RuntimeHandoffLoopMessage::TmuxReady { output_path: "/runtime/direct-claim-evicted.jsonl".into(), input_fifo_path: "/runtime/direct.input".into(), tmux_session_name: "AgentDesk-codex-direct-claim-evicted".into(), last_offset: 10_241 }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_adoption_pins_authoritative_incumbent_not_provisional_marker() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let provider = ProviderKind::Codex;
    let channel_id = 42_592_606;
    let owner = ChannelId::new(channel_id + 100);
    let tmux_session_name = "AgentDesk-codex-incarnation-pin";
    let output_path = "/runtime/incarnation-pin.jsonl";
    let mut state = runtime_seed(provider.clone(), channel_id);
    save_inflight_state(&state).expect("seed pre-handoff row");
    state = load_inflight_state(&provider, channel_id).expect("load exact pre-handoff row");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let incumbent = live_watcher_handle(tmux_session_name, output_path);
    let incumbent_delivery_pin = Arc::clone(&incumbent.turn_delivered);
    shared.tmux_watchers.insert(owner, incumbent);
    let unrelated_provisional_marker = Arc::new(AtomicBool::new(false));
    assert!(!Arc::ptr_eq(
        &incumbent_delivery_pin,
        &unrelated_provisional_marker
    ));
    let mut state_dirty = false;

    let observed = dispatch_process_handoff(
        &shared,
        &provider,
        &mut state,
        RuntimeHandoffLoopMessage::RuntimeReady {
            handoff: RuntimeHandoff::CodexTui {
                rollout_path: output_path.to_string(),
                thread_id: Some("codex-thread-incarnation-pin".to_string()),
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: 9_216,
            },
        },
        &mut state_dirty,
        false,
    )
    .await;

    assert_eq!(observed.outcome, Some(GuardedSaveOutcome::Saved));
    assert_eq!(
        observed.claim_outcome,
        WatcherHandoffClaimOutcome::ReusedExisting
    );
    assert_eq!(observed.watcher_owner_channel_id, owner);
    let pinned = observed
        .watcher_delivery_pin
        .as_ref()
        .expect("runtime adoption must publish a watcher incarnation pin");
    assert!(Arc::ptr_eq(pinned, &incumbent_delivery_pin));
    assert!(!Arc::ptr_eq(pinned, &unrelated_provisional_marker));
    let registry = shared
        .tmux_watchers
        .get(&owner)
        .expect("incumbent survives reuse");
    assert!(Arc::ptr_eq(pinned, &registry.turn_delivered));
}

#[tokio::test]
async fn thread_follow_up_tmux_ready_claim_records_intended_classification_4984() {
    let _config_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _observability_lock = crate::services::observability::test_runtime_lock();
    crate::services::observability::reset_for_tests();
    let root = tempfile::tempdir().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let provider = ProviderKind::Claude;
    let parent_channel_id = ChannelId::new(42_592_699);
    let thread_channel_id = 42_592_700;
    let tmux_session_name = "AgentDesk-claude-4984-thread-follow-up";
    let output_path = "/runtime/4984-thread-follow-up.jsonl";
    let mut state = runtime_seed(provider.clone(), thread_channel_id);
    state.logical_channel_id = Some(parent_channel_id.get());
    state.thread_id = Some(thread_channel_id);
    save_inflight_state(&state).expect("seed thread inflight row");
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.tmux_watchers.insert(
        parent_channel_id,
        live_watcher_handle(tmux_session_name, output_path),
    );
    let mut state_dirty = false;

    let observed = dispatch_process_handoff(
        &shared,
        &provider,
        &mut state,
        RuntimeHandoffLoopMessage::TmuxReady {
            output_path: output_path.to_string(),
            input_fifo_path: "/runtime/4984-thread-follow-up.input".to_string(),
            tmux_session_name: tmux_session_name.to_string(),
            last_offset: 0,
        },
        &mut state_dirty,
        false,
    )
    .await;

    assert_eq!(observed.outcome, Some(GuardedSaveOutcome::Saved));
    assert_eq!(observed.watcher_owner_channel_id, parent_channel_id);
    let classification = crate::services::observability::events::recent(20)
        .into_iter()
        .find(|event| {
            event.event_type == "invariant_violation"
                && event.payload["invariant"] == "watcher_cross_channel_tmux_claim_observed"
        })
        .expect("runtime handoff must persist the intended thread follow-up classification");
    assert_eq!(
        classification.payload["details"]["claim_classification"],
        "intended_thread_follow_up"
    );
    assert_eq!(
        classification.payload["details"]["thread_parent_provenance"],
        "persisted_inflight"
    );
}

#[tokio::test]
async fn process_ready_skips_reowned_row_and_does_not_queue_stale_flush() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let provider = ProviderKind::Claude;
    let mut state = runtime_seed(provider.clone(), 42_592_602);
    let mut newer = state.clone();
    newer.user_msg_id = 99_999;
    newer.output_path = Some("/runtime/newer-turn.jsonl".to_string());
    save_inflight_state(&newer).expect("seed re-owned row");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mut state_dirty = false;

    let outcome = dispatch_process_handoff(
        &shared,
        &provider,
        &mut state,
        RuntimeHandoffLoopMessage::ProcessReady {
            output_path: "/runtime/stale-process.jsonl".to_string(),
            session_name: "stale-process-session".to_string(),
            last_offset: 8192,
        },
        &mut state_dirty,
        false,
    )
    .await;

    assert_eq!(outcome.outcome, Some(GuardedSaveOutcome::IdentityMismatch));
    assert!(outcome.retry_message.is_none());
    assert!(
        !state_dirty,
        "a stale handoff must not queue a later whole-row flush"
    );
    let persisted = load_inflight_state(&provider, state.channel_id).expect("preserved row");
    assert_eq!(persisted.user_msg_id, 99_999);
    assert_eq!(
        persisted.output_path.as_deref(),
        Some("/runtime/newer-turn.jsonl")
    );
}

async fn assert_reowned_watcher_handoff_has_no_side_effects(
    message: RuntimeHandoffLoopMessage,
    channel_id: u64,
) {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let provider = ProviderKind::Codex;
    let mut stale = runtime_seed(provider.clone(), channel_id);
    let mut successor = stale.clone();
    successor.user_msg_id = 99_999;
    successor.output_path = Some("/runtime/successor.jsonl".to_string());
    save_inflight_state(&successor).expect("seed successor row");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mut state_dirty = false;

    let observed = dispatch_process_handoff(
        &shared,
        &provider,
        &mut stale,
        message,
        &mut state_dirty,
        false,
    )
    .await;

    assert_eq!(observed.outcome, Some(GuardedSaveOutcome::IdentityMismatch));
    assert!(observed.retry_message.is_none());
    assert_eq!(observed.claim_outcome, WatcherHandoffClaimOutcome::None);
    assert!(!observed.tmux_handed_off);
    assert!(!observed.watcher_relay_available);
    assert_eq!(observed.watcher_slots, 0);
    assert!(!state_dirty);
}

#[tokio::test]
async fn legacy_tmux_ready_reowned_row_never_claims_or_starts_watcher() {
    assert_reowned_watcher_handoff_has_no_side_effects(
        RuntimeHandoffLoopMessage::TmuxReady {
            output_path: "/runtime/stale-legacy.jsonl".to_string(),
            input_fifo_path: "/runtime/stale-legacy.input".to_string(),
            tmux_session_name: "AgentDesk-codex-stale-legacy".to_string(),
            last_offset: 4_096,
        },
        42_592_603,
    )
    .await;
}

#[tokio::test]
async fn runtime_ready_reowned_row_never_claims_or_starts_watcher() {
    assert_reowned_watcher_handoff_has_no_side_effects(
        RuntimeHandoffLoopMessage::RuntimeReady {
            handoff: RuntimeHandoff::CodexTui {
                rollout_path: "/runtime/stale-codex.jsonl".to_string(),
                thread_id: None,
                tmux_session_name: "AgentDesk-codex-stale-runtime".to_string(),
                last_offset: 8_192,
            },
        },
        42_592_604,
    )
    .await;
}

#[cfg(unix)]
#[test]
fn provisional_cleanup_preserves_replacement_incarnation() {
    let provider = ProviderKind::Codex;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let owner = ChannelId::new(42_592_608);
    let tmux_session_name = "AgentDesk-codex-cleanup-race";
    let output_path = "/runtime/cleanup-race.jsonl";
    let provisional = live_watcher_handle(tmux_session_name, output_path);
    let provisional_cancel = Arc::clone(&provisional.cancel);
    shared.tmux_watchers.insert(owner, provisional);

    let replacement = live_watcher_handle(tmux_session_name, output_path);
    let replacement_cancel = Arc::clone(&replacement.cancel);
    let replacement_claim = crate::services::discord::tmux::claim_or_replace_watcher(
        &shared.tmux_watchers,
        owner,
        replacement,
        &provider,
        "turn_bridge_runtime_cleanup_race",
    );
    assert!(replacement_claim.should_spawn());
    assert!(provisional_cancel.load(Ordering::Relaxed));
    assert!(!replacement_cancel.load(Ordering::Relaxed));

    cancel_provisional_watcher_claim_if_matches(
        shared.as_ref(),
        owner,
        tmux_session_name,
        output_path,
        &provisional_cancel,
    );

    let registered = shared
        .tmux_watchers
        .get(&owner)
        .expect("replacement must survive stale provisional cleanup");
    assert!(Arc::ptr_eq(&registered.cancel, &replacement_cancel));
    assert!(!registered.cancel.load(Ordering::Relaxed));
    assert!(provisional_cancel.load(Ordering::Relaxed));
}
