use super::super::relay_health::RelayStallClassifier;
use super::*;
use crate::services::provider::{CancelToken, ProviderKind};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[path = "tests/circuit_breaker_apply.rs"]
mod circuit_breaker_apply;

fn isolated_agentdesk_root() -> (AgentdeskRootGuard, tempfile::TempDir) {
    let temp = tempfile::TempDir::new().unwrap();
    let lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let guard = AgentdeskRootGuard {
        previous: std::env::var_os("AGENTDESK_ROOT_DIR"),
        _lock: lock,
    };
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
    (guard, temp)
}

async fn registry_with_shared(provider: ProviderKind) -> (HealthRegistry, Arc<SharedData>) {
    let registry = HealthRegistry::new();
    let shared = super::super::make_shared_data_for_tests();
    registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;
    (registry, shared)
}

async fn start_test_turn(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    message: MessageId,
) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    let started = super::super::mailbox_try_start_turn(
        shared,
        channel,
        token.clone(),
        UserId::new(1),
        message,
    )
    .await;
    assert!(started, "test mailbox turn should start on an idle channel");
    token
}

fn test_watcher_handle(
    tmux_session_name: &str,
    output_path: &std::path::Path,
) -> (
    super::super::TmuxWatcherHandle,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    (
        super::super::TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: output_path.to_string_lossy().to_string(),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: cancel.clone(),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                super::super::tmux_watcher_now_ms(),
            )),
        },
        cancel,
    )
}

fn snapshot() -> RelayHealthSnapshot {
    RelayHealthSnapshot {
        provider: "codex".to_string(),
        channel_id: 42,
        active_turn: RelayActiveTurn::None,
        tmux_session: None,
        tmux_alive: None,
        watcher_attached: false,
        watcher_attached_stale: false,
        watcher_owner_channel_id: None,
        watcher_owns_live_relay: false,
        bridge_inflight_present: false,
        bridge_current_msg_id: None,
        mailbox_has_cancel_token: false,
        mailbox_active_user_msg_id: None,
        mailbox_turn_started_at_ms: None,
        queue_depth: 0,
        pending_discord_callback_msg_id: None,
        pending_thread_proof: false,
        parent_channel_id: None,
        thread_channel_id: None,
        last_relay_ts_ms: None,
        last_outbound_activity_ms: None,
        last_capture_offset: None,
        last_relay_offset: 0,
        unread_bytes: None,
        desynced: false,
        stale_thread_proof: false,
    }
}

#[test]
fn relay_recovery_takeover_forgets_registered_completion_footer_target() {
    let channel_id = ChannelId::new(3_089_203);
    let shared = super::super::make_shared_data_for_tests();
    super::super::footer_view_reconciler::completion_footer_forget_registered_target(channel_id);
    let _ = super::super::footer_view_reconciler::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_303),
        &ProviderKind::Codex,
        1_800_000_000,
        "Final answer",
        None,
        true,
    );

    assert!(completion_footer::forget_if_message(
        channel_id,
        Some(3_089_303),
    ));

    assert_eq!(
        super::super::footer_view_reconciler::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            1_800_000_005,
        ),
        None
    );
}

#[test]
fn relay_recovery_takeover_keeps_different_completion_footer_target() {
    let channel_id = ChannelId::new(3_089_213);
    let shared = super::super::make_shared_data_for_tests();
    super::super::footer_view_reconciler::completion_footer_forget_registered_target(channel_id);
    let _ = super::super::footer_view_reconciler::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_313),
        &ProviderKind::Codex,
        1_800_000_000,
        "Final answer",
        None,
        true,
    );

    assert!(!completion_footer::forget_if_message(
        channel_id,
        Some(3_089_314),
    ));

    assert!(
        super::super::footer_view_reconciler::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            1_800_000_005,
        )
        .is_some()
    );
    super::super::footer_view_reconciler::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn dry_run_plans_safe_stale_thread_proof_cleanup() {
    let decision = plan_relay_recovery(
        &RelayHealthSnapshot {
            pending_thread_proof: true,
            stale_thread_proof: true,
            thread_channel_id: Some(99),
            ..snapshot()
        },
        RelayStallState::StaleThreadProof,
        1_000,
    );

    assert_eq!(
        decision.action,
        RelayRecoveryActionKind::ClearStaleThreadProof
    );
    assert!(decision.auto_heal.eligible);
    assert_eq!(decision.affected.thread_channel_id, Some(99));
    assert_eq!(
        decision.auto_heal.remaining_attempts,
        AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW
    );
}

#[test]
fn active_foreground_stream_is_observe_only() {
    let decision = plan_relay_recovery(
        &RelayHealthSnapshot {
            active_turn: RelayActiveTurn::Foreground,
            mailbox_has_cancel_token: true,
            bridge_inflight_present: true,
            ..snapshot()
        },
        RelayStallState::ActiveForegroundStream,
        1_000,
    );

    assert_eq!(decision.action, RelayRecoveryActionKind::ObserveOnly);
    assert!(!decision.auto_heal.eligible);
    assert_eq!(
        decision.auto_heal.skipped_reason,
        Some("live_foreground_turn")
    );
}

#[test]
fn queue_blocked_schedules_bounded_pending_queue_drain_when_idle() {
    let decision = plan_relay_recovery(
        &RelayHealthSnapshot {
            queue_depth: 2,
            ..snapshot()
        },
        RelayStallState::QueueBlocked,
        1_000,
    );

    assert_eq!(decision.action, RelayRecoveryActionKind::DrainPendingQueue);
    assert_eq!(
        decision.reason,
        "queued work is stranded behind an idle mailbox; bounded queue drain can restore delivery"
    );
    assert!(decision.auto_heal.eligible);
    assert_eq!(decision.auto_heal.skipped_reason, None);
}

#[test]
fn queue_blocked_allows_disk_backed_queue_to_reach_drain_helper() {
    let decision = plan_relay_recovery(
        &RelayHealthSnapshot {
            queue_depth: 0,
            ..snapshot()
        },
        RelayStallState::QueueBlocked,
        1_000,
    );

    assert_eq!(decision.action, RelayRecoveryActionKind::DrainPendingQueue);
    assert!(
        decision.auto_heal.eligible,
        "disk-backed pending queues are hydrated by the drain helper"
    );
    assert_eq!(decision.auto_heal.skipped_reason, None);
}

#[test]
fn queue_blocked_does_not_drain_when_live_turn_evidence_remains() {
    let decision = plan_relay_recovery(
        &RelayHealthSnapshot {
            active_turn: RelayActiveTurn::Foreground,
            mailbox_has_cancel_token: true,
            queue_depth: 2,
            ..snapshot()
        },
        RelayStallState::QueueBlocked,
        1_000,
    );

    assert_eq!(decision.action, RelayRecoveryActionKind::DrainPendingQueue);
    assert!(!decision.auto_heal.eligible);
    assert_eq!(
        decision.auto_heal.skipped_reason,
        Some("queue_blocked_has_live_turn_evidence")
    );
}

#[test]
fn live_agentdesk_tmux_relay_dead_can_reattach_watcher_when_evidence_is_complete() {
    let decision = plan_relay_recovery(
        &RelayHealthSnapshot {
            tmux_session: Some("AgentDesk-codex-42".to_string()),
            tmux_alive: Some(true),
            desynced: true,
            bridge_inflight_present: true,
            mailbox_has_cancel_token: true,
            ..snapshot()
        },
        RelayStallState::TmuxAliveRelayDead,
        1_000,
    );

    assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);
    assert!(decision.auto_heal.eligible);
    assert_eq!(decision.auto_heal.skipped_reason, None);
}

#[test]
fn watcher_owned_live_relay_with_unread_bytes_and_zero_relay_offset_is_actionable() {
    let snapshot = RelayHealthSnapshot {
        provider: "claude".to_string(),
        channel_id: 1509350393350459434,
        active_turn: RelayActiveTurn::Foreground,
        tmux_session: Some("AgentDesk-claude-adk-claude-pipe-e2e".to_string()),
        tmux_alive: Some(true),
        watcher_attached: true,
        watcher_owner_channel_id: Some(1509350393350459434),
        watcher_owns_live_relay: true,
        bridge_inflight_present: true,
        mailbox_has_cancel_token: true,
        mailbox_active_user_msg_id: Some(9001),
        mailbox_turn_started_at_ms: None,
        bridge_current_msg_id: Some(9002),
        last_capture_offset: Some(7968),
        last_relay_offset: 0,
        unread_bytes: Some(7968),
        desynced: true,
        ..snapshot()
    };
    let relay_stall_state = RelayStallClassifier::classify(&snapshot);
    let decision = plan_relay_recovery(&snapshot, relay_stall_state, 1_000);

    assert_eq!(relay_stall_state, RelayStallState::TmuxAliveRelayDead);
    assert_ne!(decision.action, RelayRecoveryActionKind::ObserveOnly);
    assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);
    assert!(
        decision.auto_heal.eligible,
        "zero-frontier unread relay-dead turns must be eligible for bounded reattach"
    );
    assert_eq!(decision.auto_heal.skipped_reason, None);
    assert!(
        decision.auto_heal.bounded,
        "relay-dead foreground turns must surface bounded recovery metadata"
    );
    assert_eq!(decision.provider, "claude");
    assert_eq!(decision.channel_id, 1509350393350459434);
    assert_eq!(
        decision.affected.tmux_session.as_deref(),
        Some("AgentDesk-claude-adk-claude-pipe-e2e")
    );
    assert_eq!(decision.evidence.unread_bytes, Some(7968));
    assert_eq!(decision.evidence.last_capture_offset, Some(7968));
    assert_eq!(decision.evidence.last_relay_offset, 0);
    assert_eq!(
        decision.evidence.watcher_owner_channel_id,
        Some(1509350393350459434)
    );
    assert!(decision.evidence.watcher_owns_live_relay);
    assert_eq!(decision.evidence.active_turn, RelayActiveTurn::Foreground);
    assert_eq!(
        relay_frontier_dead_reattach_owner(&decision),
        Some(ChannelId::new(1509350393350459434))
    );
}

#[test]
fn watcher_owned_live_relay_with_relay_progress_is_not_destructive_cancel_candidate() {
    let snapshot = RelayHealthSnapshot {
        provider: "claude".to_string(),
        channel_id: 1509350393350459434,
        active_turn: RelayActiveTurn::Foreground,
        tmux_session: Some("AgentDesk-claude-adk-claude-pipe-e2e".to_string()),
        tmux_alive: Some(true),
        watcher_attached: true,
        watcher_owner_channel_id: Some(1509350393350459434),
        watcher_owns_live_relay: true,
        bridge_inflight_present: true,
        mailbox_has_cancel_token: true,
        mailbox_active_user_msg_id: Some(9001),
        mailbox_turn_started_at_ms: None,
        bridge_current_msg_id: Some(9002),
        last_relay_ts_ms: Some(1_777_001_234_000),
        last_capture_offset: Some(7968),
        last_relay_offset: 4096,
        unread_bytes: Some(3872),
        desynced: true,
        ..snapshot()
    };
    let relay_stall_state = RelayStallClassifier::classify(&snapshot);
    let decision = plan_relay_recovery(&snapshot, relay_stall_state, 1_000);

    assert_eq!(relay_stall_state, RelayStallState::ActiveForegroundStream);
    assert_eq!(decision.action, RelayRecoveryActionKind::ObserveOnly);
    assert_eq!(relay_frontier_dead_reattach_owner(&decision), None);

    let forced_dead_decision =
        plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
    assert_eq!(
        relay_frontier_dead_reattach_owner(&forced_dead_decision),
        None,
        "a nonzero relay frontier is progress evidence; even if a later snapshot is \
             relay-dead, recovery must use non-destructive rebind instead of destructive cancel"
    );
}

/// #3277 (Defect D) + deploy-preserved ownerless restore eligibility table:
/// a DEAD attached watcher handle (`watcher_attached_stale`) no longer
/// blocks the bounded reattach; a genuinely-live watcher that already owns
/// the relay still does; and a live-but-ownerless watcher is eligible
/// because rebind only needs to restamp ownership and reuse the incumbent.
#[test]
fn reattach_eligibility_distinguishes_stale_attached_watcher_from_live() {
    let base = || RelayHealthSnapshot {
        tmux_session: Some("AgentDesk-claude-adk-cc".to_string()),
        tmux_alive: Some(true),
        desynced: true,
        bridge_inflight_present: true,
        mailbox_has_cancel_token: true,
        ..snapshot()
    };

    // attached + stale handle → dead-handle evidence → eligible.
    let stale_attached = plan_relay_recovery(
        &RelayHealthSnapshot {
            watcher_attached: true,
            watcher_attached_stale: true,
            ..base()
        },
        RelayStallState::TmuxAliveRelayDead,
        1_000,
    );
    assert_eq!(
        stale_attached.action,
        RelayRecoveryActionKind::ReattachWatcher
    );
    assert!(
        stale_attached.auto_heal.eligible,
        "a cancelled/heartbeat-stale attached watcher must not block reattach"
    );
    assert_eq!(stale_attached.auto_heal.skipped_reason, None);

    // attached + LIVE ownerless handle → eligible; this is the
    // post-deploy `watcher_attached=true` / `watcher_owns_live_relay=false`
    // gap where the handle exists but cannot relay the current inflight.
    let live_ownerless_attached = plan_relay_recovery(
        &RelayHealthSnapshot {
            watcher_attached: true,
            watcher_attached_stale: false,
            ..base()
        },
        RelayStallState::TmuxAliveRelayDead,
        1_000,
    );
    assert!(
        live_ownerless_attached.auto_heal.eligible,
        "a live but ownerless watcher should be reused and restamped by reattach"
    );
    assert_eq!(live_ownerless_attached.auto_heal.skipped_reason, None);

    // attached + LIVE owner → never auto-replace a live relay owner.
    let live_owned_attached = plan_relay_recovery(
        &RelayHealthSnapshot {
            watcher_attached: true,
            watcher_attached_stale: false,
            watcher_owns_live_relay: true,
            ..base()
        },
        RelayStallState::TmuxAliveRelayDead,
        1_000,
    );
    assert!(
        !live_owned_attached.auto_heal.eligible,
        "a fresh-heartbeat live watcher that owns relay must keep reattach operator-gated"
    );
    assert_eq!(
        live_owned_attached.auto_heal.skipped_reason,
        Some("reattach_missing_required_live_evidence")
    );

    // detached (legacy case) → still eligible, unchanged.
    let detached = plan_relay_recovery(&base(), RelayStallState::TmuxAliveRelayDead, 1_000);
    assert!(detached.auto_heal.eligible);
}

/// #3277 verify-2: the reattach apply reports HONESTLY whether a watcher
/// was actually spawned (dead incumbent replaced / fresh claim) or a live
/// same-session incumbent was reused untouched — the latter must not be
/// labelled "reattached_watcher".
#[test]
fn reattach_status_reports_live_incumbent_reuse_honestly() {
    assert_eq!(reattach_apply_status(true), "reattached_watcher");
    assert_eq!(reattach_apply_status(false), "reuse_existing_live_watcher");
}

#[test]
fn live_agentdesk_tmux_relay_dead_without_mailbox_token_can_adopt_ownerless_inflight() {
    let decision = plan_relay_recovery(
        &RelayHealthSnapshot {
            tmux_session: Some("AgentDesk-codex-42".to_string()),
            tmux_alive: Some(true),
            desynced: true,
            bridge_inflight_present: true,
            mailbox_has_cancel_token: false,
            ..snapshot()
        },
        RelayStallState::TmuxAliveRelayDead,
        1_000,
    );

    assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);
    assert!(decision.auto_heal.eligible);
    assert_eq!(decision.auto_heal.skipped_reason, None);
}

#[tokio::test]
async fn dead_frontier_watcher_cancel_finalizes_owner_and_releases_inflight() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(4_030_001);
    let user_msg = MessageId::new(4_030_101);
    let tmux = "AgentDesk-codex-4030-dead-frontier";
    let output_path = root_dir.path().join("watcher-output.jsonl");
    std::fs::write(&output_path, r#"{"type":"thread.started","thread_id":"t"}"#)
        .expect("write output fixture");
    let output_len = std::fs::metadata(&output_path)
        .expect("output fixture metadata")
        .len();
    let token = start_test_turn(&shared, channel, user_msg).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);

    let mut state = super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        user_msg.get(),
        4_030_201,
        "watcher-owned turn".to_string(),
        None,
        Some(tmux.to_string()),
        Some(output_path.to_string_lossy().to_string()),
        None,
        output_len,
    );
    state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
    state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
    super::super::inflight::save_inflight_state(&state).expect("save watcher inflight");
    shared.turn_finalizer.register_start(
        super::super::turn_finalizer::TurnKey::new(
            channel,
            state.effective_finalizer_turn_id(),
            shared.restart.current_generation,
        ),
        provider.clone(),
        super::super::inflight::RelayOwnerKind::Watcher,
        &shared,
    );
    let (watcher, watcher_cancel) = test_watcher_handle(tmux, &output_path);
    watcher.last_heartbeat_ts_ms.store(1, Ordering::Release);
    shared.tmux_watchers.insert(channel, watcher);

    let snapshot = RelayHealthSnapshot {
        provider: provider.as_str().to_string(),
        channel_id: channel.get(),
        active_turn: RelayActiveTurn::Foreground,
        tmux_session: Some(tmux.to_string()),
        tmux_alive: Some(true),
        watcher_attached: true,
        watcher_owner_channel_id: Some(channel.get()),
        watcher_owns_live_relay: true,
        bridge_inflight_present: true,
        mailbox_has_cancel_token: true,
        mailbox_active_user_msg_id: Some(user_msg.get()),
        mailbox_turn_started_at_ms: None,
        last_capture_offset: Some(128),
        last_relay_offset: 0,
        unread_bytes: Some(128),
        desynced: true,
        ..snapshot()
    };
    let mut decision = plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
    decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());

    let _ = apply_relay_recovery_decision(
        &registry,
        &shared,
        &provider,
        &decision,
        None,
        RelayRecoveryApplySource::ProbeAutoHeal,
    )
    .await;

    assert!(
        watcher_cancel.load(Ordering::Relaxed),
        "relay recovery must still cancel the dead-frontier watcher"
    );
    assert!(
        token.cancelled.load(Ordering::Relaxed),
        "watcher cancel must release the owning mailbox token through the finalizer"
    );
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    assert!(
        super::super::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_none(),
        "finalizer-routed watcher cancel must clear active mailbox ownership"
    );
    assert!(
        super::super::inflight::load_inflight_state(&provider, channel.get()).is_none(),
        "finalizer-routed watcher cancel must clear the owning inflight row"
    );
}

#[tokio::test]
async fn reattach_idle_tmux_clear_release_publishes_completion_event() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Claude;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(4_048_410);
    let user_msg = MessageId::new(4_048_411);
    let tmux = "AgentDesk-claude-4048-reattach-idle-clear";
    let output_path = root_dir.path().join("idle-clear-ready.jsonl");
    let body = "{\"type\":\"system\",\"subtype\":\"turn_duration\",\"session_id\":\"s\"}\n";
    std::fs::write(&output_path, body).expect("write ready output fixture");
    let output_len = std::fs::metadata(&output_path)
        .expect("output fixture metadata")
        .len();
    let token = start_test_turn(&shared, channel, user_msg).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);

    let mut state = super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        user_msg.get(),
        4_048_412,
        "idle tmux cleanup".to_string(),
        None,
        Some(tmux.to_string()),
        Some(output_path.to_string_lossy().to_string()),
        None,
        output_len,
    );
    state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
    super::super::inflight::save_inflight_state(&state).expect("save idle-clear inflight");

    let snapshot = RelayHealthSnapshot {
        provider: provider.as_str().to_string(),
        channel_id: channel.get(),
        active_turn: RelayActiveTurn::Foreground,
        tmux_session: Some(tmux.to_string()),
        tmux_alive: Some(true),
        bridge_inflight_present: true,
        mailbox_has_cancel_token: true,
        mailbox_active_user_msg_id: Some(user_msg.get()),
        mailbox_turn_started_at_ms: None,
        last_capture_offset: Some(output_len),
        last_relay_offset: output_len,
        unread_bytes: Some(0),
        desynced: true,
        ..snapshot()
    };
    let decision = plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
    assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);
    assert!(idle_tmux_repair_ready_for_input(
        &provider,
        channel.get(),
        tmux
    ));
    assert!(
        !idle_tmux_repair_has_unrelayed_tail_answer(&state),
        "consumed-at-EOF terminal JSONL must not block idle-tmux cleanup"
    );

    let mut rx =
        super::super::turn_completion_events::subscribe_turn_completion_events(shared.as_ref());
    let result = apply_relay_recovery_decision(
        &registry,
        &shared,
        &provider,
        &decision,
        None,
        RelayRecoveryApplySource::ProbeAutoHeal,
    )
    .await;

    assert_eq!(result.status, "cleared_idle_tmux_stale_turn");
    assert!(result.removed_mailbox_token);
    assert!(token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    let event = rx
        .try_recv()
        .expect("reattach idle-clear mailbox release must publish completion event");
    assert_eq!(event.channel_id, channel);
    assert_eq!(
        shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
        0,
        "release primitive publishes only; the queue listener owns drain/backstop policy"
    );
    assert!(
        super::super::inflight::load_inflight_state(&provider, channel.get()).is_none(),
        "idle-tmux cleanup must clear stale inflight after publishing the release edge"
    );
}

#[test]
fn reattach_idle_tmux_clear_generation_guard_preserves_concurrent_inflight_update() {
    let _guard = auto_heal_test_lock().blocking_lock();
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(4_111_003);
    let user_msg = MessageId::new(4_111_103);
    let tmux = "AgentDesk-claude-4111-idle-clear-generation";
    let output_path = root_dir.path().join("idle-clear-generation.jsonl");
    let body = "{\"type\":\"system\",\"subtype\":\"turn_duration\",\"session_id\":\"s\"}\n";
    std::fs::write(&output_path, body).expect("write ready output fixture");

    let mut state = super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        user_msg.get(),
        4_111_203,
        "idle tmux guarded cleanup".to_string(),
        Some("session-4111-idle-clear".to_string()),
        Some(tmux.to_string()),
        Some(output_path.to_string_lossy().to_string()),
        None,
        body.len() as u64,
    );
    state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
    super::super::inflight::save_inflight_state(&state).expect("seed stale idle-clear row");

    let pin = capture_idle_tmux_reattach_inflight_clear_pin(&state)
        .expect("capture clear pin before concurrent writer");
    let mut concurrent = super::super::inflight::load_inflight_state(&provider, channel.get())
        .expect("seeded row for concurrent update");
    concurrent.last_watcher_relayed_offset = Some(8_192);
    concurrent.last_watcher_relayed_generation_mtime_ns = Some(77_777);
    concurrent.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::SessionBoundRelay);
    super::super::inflight::save_inflight_state(&concurrent)
        .expect("save concurrent generation-advancing update");

    assert_eq!(
        clear_idle_tmux_reattach_inflight_if_pinned(&provider, channel.get(), Some(&pin)),
        super::super::inflight::GuardedClearOutcome::UserMsgMismatch,
        "auto reattach idle clear must fail closed when the row save_generation advanced"
    );
    let persisted = super::super::inflight::load_inflight_state(&provider, channel.get())
        .expect("advanced row must survive stale generation clear");
    assert_eq!(persisted.last_watcher_relayed_offset, Some(8_192));
    assert_eq!(
        persisted.last_watcher_relayed_generation_mtime_ns,
        Some(77_777)
    );
    assert_eq!(
        persisted.effective_relay_owner_kind(),
        super::super::inflight::RelayOwnerKind::SessionBoundRelay
    );
}

#[tokio::test]
async fn reattach_idle_tmux_clear_refuses_newer_idle_row_between_predicate_and_pin() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Claude;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(4_111_004);
    let channel_id = channel.get();
    let stale_user_msg_id = 4_111_104;
    let newer_user_msg_id = 4_111_204;
    let user_msg = MessageId::new(stale_user_msg_id);
    let tmux = "AgentDesk-claude-4111-idle-clear-predicate-pin";
    let output_path = root_dir.path().join("idle-clear-predicate-pin.jsonl");
    let body = "{\"type\":\"system\",\"subtype\":\"turn_duration\",\"session_id\":\"s\"}\n";
    std::fs::write(&output_path, body).expect("write ready output fixture");
    let output_len = std::fs::metadata(&output_path)
        .expect("output fixture metadata")
        .len();
    let token = start_test_turn(&shared, channel, user_msg).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);

    let mut stale = super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id,
        None,
        1,
        stale_user_msg_id,
        4_111_304,
        "stale idle tmux cleanup".to_string(),
        Some("session-4111-idle-clear-stale".to_string()),
        Some(tmux.to_string()),
        Some(output_path.to_string_lossy().to_string()),
        None,
        output_len,
    );
    stale.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
    super::super::inflight::save_inflight_state(&stale).expect("seed stale idle-clear row");

    let hook_provider = provider.clone();
    let hook_tmux = tmux.to_string();
    let hook_output_path = output_path.to_string_lossy().to_string();
    let (watcher, watcher_cancel) = test_watcher_handle(tmux, &output_path);
    shared.tmux_watchers.insert(channel, watcher);
    let footer_msg = MessageId::new(4_111_704);
    super::super::footer_view_reconciler::completion_footer_forget_registered_target(channel);
    let _ = super::super::footer_view_reconciler::register_completion_footer_target(
        channel,
        footer_msg,
        &provider,
        1_800_000_000,
        "Final answer",
        None,
        true,
    );

    let _hook = set_idle_tmux_reattach_inflight_candidate_hook_for_tests(Arc::new(
        move |predicate_snapshot| {
            assert_eq!(
                predicate_snapshot.user_msg_id, stale_user_msg_id,
                "hook must receive the stale readiness snapshot before pin capture"
            );
            let mut newer = super::super::inflight::InflightTurnState::new(
                hook_provider.clone(),
                channel_id,
                None,
                1,
                newer_user_msg_id,
                4_111_404,
                "newer idle tmux cleanup".to_string(),
                Some("session-4111-idle-clear-newer".to_string()),
                Some(hook_tmux.clone()),
                Some(hook_output_path.clone()),
                None,
                output_len,
            );
            newer.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
            super::super::inflight::save_inflight_state(&newer)
                .expect("write newer idle-shaped row before pin capture");
        },
    ));

    let snapshot = RelayHealthSnapshot {
        provider: provider.as_str().to_string(),
        channel_id,
        active_turn: RelayActiveTurn::Foreground,
        tmux_session: Some(tmux.to_string()),
        tmux_alive: Some(true),
        bridge_inflight_present: true,
        mailbox_has_cancel_token: true,
        mailbox_active_user_msg_id: Some(stale_user_msg_id),
        mailbox_turn_started_at_ms: None,
        bridge_current_msg_id: Some(footer_msg.get()),
        last_capture_offset: Some(output_len),
        last_relay_offset: output_len,
        unread_bytes: Some(0),
        desynced: true,
        ..snapshot()
    };
    let decision = plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
    assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);

    let result = apply_relay_recovery_decision(
        &registry,
        &shared,
        &provider,
        &decision,
        None,
        RelayRecoveryApplySource::ProbeAutoHeal,
    )
    .await;

    assert_eq!(
        result.status, "skipped_idle_tmux_stale_turn_pin_mismatch",
        "a refused generation-pinned clear must not report the applied clear status"
    );
    assert!(!relay_recovery_status_counts_as_applied(result.status));
    assert_ne!(result.status, "cleared_idle_tmux_stale_turn");
    assert!(!result.removed_mailbox_token);
    assert_eq!(result.post_mailbox_has_cancel_token, Some(true));
    assert!(!token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    assert!(
        shared.tmux_watchers.contains_key(&channel),
        "skipped clear must preserve the watcher binding for the next watchdog pass"
    );
    assert!(
        !watcher_cancel.load(Ordering::Relaxed),
        "skipped clear must not cancel the watcher binding"
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_some(),
        "skipped clear must preserve the active mailbox token"
    );
    let persisted = super::super::inflight::load_inflight_state(&provider, channel_id)
        .expect("newer idle-shaped row must survive stale pinned clear");
    assert_eq!(persisted.user_msg_id, newer_user_msg_id);
    assert_eq!(
        persisted.session_id.as_deref(),
        Some("session-4111-idle-clear-newer")
    );
    assert_eq!(
        persisted.effective_relay_owner_kind(),
        super::super::inflight::RelayOwnerKind::Watcher
    );
    assert!(
        super::super::footer_view_reconciler::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel,
            "progress",
            1_800_000_005,
        )
        .is_some(),
        "skipped clear must preserve the registered completion footer target"
    );
    super::super::footer_view_reconciler::completion_footer_forget_registered_target(channel);
}

#[tokio::test]
async fn reattach_idle_tmux_clear_success_tears_down_after_guarded_clear() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Claude;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(4_111_005);
    let user_msg = MessageId::new(4_111_105);
    let tmux = "AgentDesk-claude-4111-idle-clear-success";
    let output_path = root_dir.path().join("idle-clear-success.jsonl");
    let body = "{\"type\":\"system\",\"subtype\":\"turn_duration\",\"session_id\":\"s\"}\n";
    std::fs::write(&output_path, body).expect("write ready output fixture");
    let output_len = std::fs::metadata(&output_path)
        .expect("output fixture metadata")
        .len();
    let token = start_test_turn(&shared, channel, user_msg).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);

    let mut state = super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        user_msg.get(),
        4_111_305,
        "idle tmux guarded cleanup success".to_string(),
        Some("session-4111-idle-clear-success".to_string()),
        Some(tmux.to_string()),
        Some(output_path.to_string_lossy().to_string()),
        None,
        output_len,
    );
    state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
    super::super::inflight::save_inflight_state(&state).expect("seed idle-clear row");
    let (watcher, watcher_cancel) = test_watcher_handle(tmux, &output_path);
    shared.tmux_watchers.insert(channel, watcher);

    let snapshot = RelayHealthSnapshot {
        provider: provider.as_str().to_string(),
        channel_id: channel.get(),
        active_turn: RelayActiveTurn::Foreground,
        tmux_session: Some(tmux.to_string()),
        tmux_alive: Some(true),
        watcher_attached: true,
        watcher_attached_stale: true,
        watcher_owner_channel_id: Some(channel.get()),
        watcher_owns_live_relay: true,
        bridge_inflight_present: true,
        mailbox_has_cancel_token: true,
        mailbox_active_user_msg_id: Some(user_msg.get()),
        mailbox_turn_started_at_ms: None,
        last_capture_offset: Some(output_len),
        last_relay_offset: output_len,
        unread_bytes: Some(0),
        desynced: true,
        ..snapshot()
    };
    let decision = plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
    assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);

    let result = apply_relay_recovery_decision(
        &registry,
        &shared,
        &provider,
        &decision,
        None,
        RelayRecoveryApplySource::ProbeAutoHeal,
    )
    .await;

    assert_eq!(result.status, "cleared_idle_tmux_stale_turn");
    assert!(relay_recovery_status_counts_as_applied(result.status));
    assert!(result.removed_mailbox_token);
    assert_eq!(result.post_mailbox_has_cancel_token, Some(false));
    assert_eq!(result.reattach_watcher_replaced, Some(true));
    assert!(token.cancelled.load(Ordering::Relaxed));
    assert!(watcher_cancel.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    assert!(
        !shared.tmux_watchers.contains_key(&channel),
        "successful guarded clear must remove the retired watcher binding"
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_none(),
        "successful guarded clear must release the mailbox token"
    );
    assert!(
        super::super::inflight::load_inflight_state(&provider, channel.get()).is_none(),
        "successful guarded clear must remove the pinned inflight row before teardown completes"
    );
}

#[tokio::test]
async fn stale_watcher_with_jsonl_progress_rebinds_without_canceling_turn() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(4_030_004);
    let user_msg = MessageId::new(4_030_104);
    let tmux = "AgentDesk-codex-4030-jsonl-progress";
    let output_path = root_dir.path().join("jsonl-progress.jsonl");
    std::fs::write(&output_path, "chunk-1").expect("write output fixture");
    let token = start_test_turn(&shared, channel, user_msg).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);

    let mut state = super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        user_msg.get(),
        4_030_204,
        "watcher-owned active turn".to_string(),
        None,
        Some(tmux.to_string()),
        Some(output_path.to_string_lossy().to_string()),
        None,
        0,
    );
    state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
    super::super::inflight::save_inflight_state(&state).expect("save watcher inflight");
    shared.turn_finalizer.register_start(
        super::super::turn_finalizer::TurnKey::new(
            channel,
            state.effective_finalizer_turn_id(),
            shared.restart.current_generation,
        ),
        provider.clone(),
        super::super::inflight::RelayOwnerKind::Watcher,
        &shared,
    );
    let (watcher, _) = test_watcher_handle(tmux, &output_path);
    watcher.last_heartbeat_ts_ms.store(1, Ordering::Release);
    shared.tmux_watchers.insert(channel, watcher);

    let snapshot = RelayHealthSnapshot {
        provider: provider.as_str().to_string(),
        channel_id: channel.get(),
        active_turn: RelayActiveTurn::Foreground,
        tmux_session: Some(tmux.to_string()),
        tmux_alive: Some(true),
        watcher_attached: true,
        watcher_owner_channel_id: Some(channel.get()),
        watcher_owns_live_relay: true,
        bridge_inflight_present: true,
        mailbox_has_cancel_token: true,
        mailbox_active_user_msg_id: Some(user_msg.get()),
        mailbox_turn_started_at_ms: None,
        last_capture_offset: Some(128),
        last_relay_offset: 0,
        unread_bytes: Some(128),
        desynced: true,
        ..snapshot()
    };
    let mut decision = plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
    decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());

    let output_for_task = output_path.clone();
    let progress = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        std::fs::write(&output_for_task, "chunk-1\nchunk-2").expect("append output fixture");
    });
    let _ = apply_relay_recovery_decision(
        &registry,
        &shared,
        &provider,
        &decision,
        None,
        RelayRecoveryApplySource::ProbeAutoHeal,
    )
    .await;
    progress.await.expect("jsonl progress task");

    assert!(
        !token.cancelled.load(Ordering::Relaxed),
        "watcher-heartbeat stale plus active JSONL progress is not turn-death evidence"
    );
    let current = super::super::inflight::load_inflight_state(&provider, channel.get())
        .expect("active turn inflight must survive watcher rebind");
    assert_eq!(current.user_msg_id, user_msg.get());
}

#[tokio::test]
async fn fresh_watcher_heartbeat_blocks_destructive_cancel_before_reattach() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(4_030_002);
    let user_msg = MessageId::new(4_030_102);
    let tmux = "AgentDesk-codex-4030-fresh-heartbeat";
    let output_path = root_dir.path().join("fresh-heartbeat.jsonl");
    std::fs::write(&output_path, "still growing soon").expect("write output fixture");
    let token = start_test_turn(&shared, channel, user_msg).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);

    let mut state = super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        user_msg.get(),
        4_030_202,
        "watcher-owned live turn".to_string(),
        None,
        Some(tmux.to_string()),
        Some(output_path.to_string_lossy().to_string()),
        None,
        0,
    );
    state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
    super::super::inflight::save_inflight_state(&state).expect("save watcher inflight");
    shared.turn_finalizer.register_start(
        super::super::turn_finalizer::TurnKey::new(
            channel,
            state.effective_finalizer_turn_id(),
            shared.restart.current_generation,
        ),
        provider.clone(),
        super::super::inflight::RelayOwnerKind::Watcher,
        &shared,
    );
    let (watcher, watcher_cancel) = test_watcher_handle(tmux, &output_path);
    shared.tmux_watchers.insert(channel, watcher);

    let snapshot = RelayHealthSnapshot {
        provider: provider.as_str().to_string(),
        channel_id: channel.get(),
        active_turn: RelayActiveTurn::Foreground,
        tmux_session: Some(tmux.to_string()),
        tmux_alive: Some(true),
        watcher_attached: true,
        watcher_owner_channel_id: Some(channel.get()),
        watcher_owns_live_relay: true,
        bridge_inflight_present: true,
        mailbox_has_cancel_token: true,
        mailbox_active_user_msg_id: Some(user_msg.get()),
        mailbox_turn_started_at_ms: None,
        last_capture_offset: Some(128),
        last_relay_offset: 0,
        unread_bytes: Some(128),
        desynced: true,
        ..snapshot()
    };
    let mut decision = plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
    decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());

    let _ = apply_relay_recovery_decision(
        &registry,
        &shared,
        &provider,
        &decision,
        None,
        RelayRecoveryApplySource::Manual,
    )
    .await;

    assert!(
        !watcher_cancel.load(Ordering::Relaxed),
        "a fresh heartbeat watcher must never be destructively cancelled"
    );
    assert!(
        !token.cancelled.load(Ordering::Relaxed),
        "fresh-heartbeat gate must preserve the live turn's mailbox token"
    );
    let current = super::super::inflight::load_inflight_state(&provider, channel.get())
        .expect("fresh-heartbeat gate must preserve inflight");
    assert_eq!(current.user_msg_id, user_msg.get());
}

#[tokio::test]
async fn relay_recovery_identity_pin_preserves_t2_started_after_t1_snapshot() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(4_030_003);
    let t1_msg = MessageId::new(4_030_103);
    let t2_msg = MessageId::new(4_030_104);
    let tmux = "AgentDesk-codex-4030-t1-t2";
    let t1_output = root_dir.path().join("t1.jsonl");
    let t2_output = root_dir.path().join("t2.jsonl");
    std::fs::write(&t1_output, "turn one tail").expect("write t1 output fixture");
    std::fs::write(&t2_output, "turn two tail").expect("write t2 output fixture");
    let _t1_token = start_test_turn(&shared, channel, t1_msg).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);

    let mut t1_state = super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        t1_msg.get(),
        4_030_203,
        "turn one".to_string(),
        None,
        Some(tmux.to_string()),
        Some(t1_output.to_string_lossy().to_string()),
        None,
        0,
    );
    t1_state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
    super::super::inflight::save_inflight_state(&t1_state).expect("save t1 inflight");
    shared.turn_finalizer.register_start(
        super::super::turn_finalizer::TurnKey::new(
            channel,
            t1_state.effective_finalizer_turn_id(),
            shared.restart.current_generation,
        ),
        provider.clone(),
        super::super::inflight::RelayOwnerKind::Watcher,
        &shared,
    );
    let (watcher, _) = test_watcher_handle(tmux, &t1_output);
    watcher.last_heartbeat_ts_ms.store(1, Ordering::Release);
    shared.tmux_watchers.insert(channel, watcher);

    let snapshot = RelayHealthSnapshot {
        provider: provider.as_str().to_string(),
        channel_id: channel.get(),
        active_turn: RelayActiveTurn::Foreground,
        tmux_session: Some(tmux.to_string()),
        tmux_alive: Some(true),
        watcher_attached: true,
        watcher_owner_channel_id: Some(channel.get()),
        watcher_owns_live_relay: true,
        bridge_inflight_present: true,
        mailbox_has_cancel_token: true,
        mailbox_active_user_msg_id: Some(t1_msg.get()),
        mailbox_turn_started_at_ms: None,
        last_capture_offset: Some(64),
        last_relay_offset: 0,
        unread_bytes: Some(64),
        desynced: true,
        ..snapshot()
    };
    let mut decision = plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
    decision.affected.finalizer_turn_id = Some(t1_state.effective_finalizer_turn_id());

    let _ = mailbox_finish_turn(&shared, &provider, channel).await;
    let t2_token = start_test_turn(&shared, channel, t2_msg).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let mut t2_state = super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        1,
        t2_msg.get(),
        4_030_204,
        "turn two".to_string(),
        None,
        Some(tmux.to_string()),
        Some(t2_output.to_string_lossy().to_string()),
        None,
        0,
    );
    t2_state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
    super::super::inflight::save_inflight_state(&t2_state).expect("save t2 inflight");
    shared.turn_finalizer.register_start(
        super::super::turn_finalizer::TurnKey::new(
            channel,
            t2_state.effective_finalizer_turn_id(),
            shared.restart.current_generation,
        ),
        provider.clone(),
        super::super::inflight::RelayOwnerKind::Watcher,
        &shared,
    );

    let _ = apply_relay_recovery_decision(
        &registry,
        &shared,
        &provider,
        &decision,
        None,
        RelayRecoveryApplySource::Manual,
    )
    .await;

    assert!(
        !t2_token.cancelled.load(Ordering::Relaxed),
        "T1's pinned recovery decision must no-op instead of canceling T2"
    );
    let current = super::super::inflight::load_inflight_state(&provider, channel.get())
        .expect("T2 inflight must survive the stale T1 recovery apply");
    assert_eq!(current.user_msg_id, t2_msg.get());
    assert_eq!(
        super::super::mailbox_snapshot(&shared, channel)
            .await
            .active_user_message_id
            .map(|id| id.get()),
        Some(t2_msg.get())
    );
}

#[test]
fn fresh_orphan_shape_is_guarded_by_admission_grace() {
    let decision = plan_relay_recovery(
        &RelayHealthSnapshot {
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(9001),
            mailbox_turn_started_at_ms: Some(1_000),
            ..snapshot()
        },
        RelayStallState::OrphanPendingToken,
        10_000,
    );

    assert_eq!(
        decision.action,
        RelayRecoveryActionKind::ClearOrphanPendingToken
    );
    assert!(!decision.auto_heal.eligible);
    assert_eq!(
        decision.auto_heal.skipped_reason,
        Some("orphan_token_within_admission_grace")
    );
    assert_eq!(decision.evidence.mailbox_turn_started_at_ms, Some(1_000));
}

#[test]
fn old_orphan_shape_remains_auto_heal_eligible() {
    let decision = plan_relay_recovery(
        &RelayHealthSnapshot {
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(9001),
            mailbox_turn_started_at_ms: Some(1_000),
            ..snapshot()
        },
        RelayStallState::OrphanPendingToken,
        1_000 + ORPHAN_PENDING_TOKEN_ADMISSION_GRACE.as_millis() as i64,
    );

    assert_eq!(
        decision.action,
        RelayRecoveryActionKind::ClearOrphanPendingToken
    );
    assert!(decision.auto_heal.eligible);
    assert_eq!(decision.auto_heal.skipped_reason, None);
}

#[test]
fn token_only_agentdesk_tmux_with_unknown_liveness_stays_protected_after_grace() {
    let decision = plan_relay_recovery(
        &RelayHealthSnapshot {
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(9001),
            mailbox_turn_started_at_ms: Some(1_000),
            tmux_session: Some("AgentDesk-codex-token-only".to_string()),
            tmux_alive: None,
            ..snapshot()
        },
        RelayStallState::OrphanPendingToken,
        1_000 + ORPHAN_PENDING_TOKEN_ADMISSION_GRACE.as_millis() as i64,
    );

    assert!(!decision.auto_heal.eligible);
    assert_eq!(
        decision.auto_heal.skipped_reason,
        Some("protected_agentdesk_tmux_session")
    );
}

#[test]
fn token_only_agentdesk_tmux_confirmed_dead_is_reclaimed_after_grace() {
    // #4569 review regression guard: the AgentDesk-name protection must NOT
    // shield a token whose tmux the probe positively confirmed dead. Removing
    // the `tmux_alive == Some(false)` escape in
    // `eligible_orphan_pending_token_without_admission_grace` re-protects this
    // token forever and wedges the mailbox.
    let decision = plan_relay_recovery(
        &RelayHealthSnapshot {
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(9001),
            mailbox_turn_started_at_ms: Some(1_000),
            tmux_session: Some("AgentDesk-codex-token-only-dead".to_string()),
            tmux_alive: Some(false),
            ..snapshot()
        },
        RelayStallState::OrphanPendingToken,
        1_000 + ORPHAN_PENDING_TOKEN_ADMISSION_GRACE.as_millis() as i64,
    );

    assert_eq!(
        decision.action,
        RelayRecoveryActionKind::ClearOrphanPendingToken
    );
    assert!(decision.auto_heal.eligible);
    assert_eq!(decision.auto_heal.skipped_reason, None);
}

#[test]
fn orphan_token_live_evidence_and_agentdesk_tmux_stay_protected() {
    let live = plan_relay_recovery(
        &RelayHealthSnapshot {
            mailbox_has_cancel_token: true,
            mailbox_turn_started_at_ms: Some(1_000),
            watcher_attached: true,
            ..snapshot()
        },
        RelayStallState::OrphanPendingToken,
        60_000,
    );
    assert!(!live.auto_heal.eligible);
    assert_eq!(
        live.auto_heal.skipped_reason,
        Some("orphan_token_has_live_evidence")
    );

    let protected_tmux = plan_relay_recovery(
        &RelayHealthSnapshot {
            mailbox_has_cancel_token: true,
            mailbox_turn_started_at_ms: Some(1_000),
            tmux_session: Some("AgentDesk-codex-protected".to_string()),
            ..snapshot()
        },
        RelayStallState::OrphanPendingToken,
        60_000,
    );
    assert!(!protected_tmux.auto_heal.eligible);
    assert_eq!(
        protected_tmux.auto_heal.skipped_reason,
        Some("protected_agentdesk_tmux_session")
    );
}

#[tokio::test]
async fn auto_heal_attempts_are_rate_limited_per_window() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let key = auto_heal_key(
        "codex",
        42,
        RelayRecoveryActionKind::ClearOrphanPendingToken,
        RelayRecoveryApplySource::ProbeAutoHeal,
    );

    assert_eq!(
        reserve_auto_heal_attempt(&key, 1_000, AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW),
        Ok(0)
    );
    assert_eq!(
        reserve_auto_heal_attempt(&key, 2_000, AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW),
        Err("auto_heal_rate_limited")
    );
    assert_eq!(
        reserve_auto_heal_attempt(
            &key,
            1_000 + AUTO_HEAL_WINDOW_SECS * 1000,
            AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW
        ),
        Ok(0)
    );
}

#[tokio::test]
async fn dead_frontier_reattach_gets_one_bounded_retry_only() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let snapshot = RelayHealthSnapshot {
        provider: "codex".to_string(),
        channel_id: 3_779_001,
        active_turn: RelayActiveTurn::Foreground,
        tmux_session: Some("AgentDesk-codex-retry-dead-frontier".to_string()),
        tmux_alive: Some(true),
        watcher_attached: true,
        watcher_owner_channel_id: Some(3_779_001),
        watcher_owns_live_relay: true,
        bridge_inflight_present: true,
        mailbox_has_cancel_token: true,
        mailbox_active_user_msg_id: Some(3_779_101),
        mailbox_turn_started_at_ms: None,
        last_capture_offset: Some(2_048),
        last_relay_offset: 0,
        unread_bytes: Some(2_048),
        desynced: true,
        ..snapshot()
    };
    let decision = plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
    let key = auto_heal_key(
        &decision.provider,
        decision.channel_id,
        decision.action,
        RelayRecoveryApplySource::Manual,
    );

    assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);
    assert!(decision.auto_heal.eligible);
    assert_eq!(
        decision.auto_heal.max_attempts_per_window,
        AUTO_HEAL_DEAD_FRONTIER_REATTACH_MAX_ATTEMPTS_PER_WINDOW
    );
    assert_eq!(decision.auto_heal.remaining_attempts, 2);
    assert_eq!(
        reserve_auto_heal_attempt(&key, 1_000, decision.auto_heal.max_attempts_per_window),
        Ok(1)
    );
    assert_eq!(
        reserve_auto_heal_attempt(&key, 2_000, decision.auto_heal.max_attempts_per_window),
        Ok(0),
        "a still-dead relay frontier gets one bounded non-destructive reattach retry"
    );
    assert_eq!(
        reserve_auto_heal_attempt(&key, 3_000, decision.auto_heal.max_attempts_per_window),
        Err("auto_heal_rate_limited")
    );

    let progressed = plan_relay_recovery(
        &RelayHealthSnapshot {
            last_relay_ts_ms: Some(2_500),
            last_relay_offset: 512,
            unread_bytes: Some(1_536),
            ..snapshot
        },
        RelayStallState::TmuxAliveRelayDead,
        3_000,
    );
    assert_eq!(
        progressed.auto_heal.max_attempts_per_window, AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW,
        "once the relay frontier advances, reattach returns to the default limiter"
    );
    assert_eq!(progressed.auto_heal.remaining_attempts, 0);
}

#[tokio::test]
async fn auto_apply_preserves_fresh_admission_token() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(3_360_001);
    let token = start_test_turn(&shared, channel, MessageId::new(91)).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);

    let response = auto_apply_relay_recovery_for_shared(
        &registry,
        shared.clone(),
        &provider,
        channel.get(),
        RelayRecoveryActionKind::ClearOrphanPendingToken,
        RelayRecoveryApplySource::ProbeAutoHeal,
    )
    .await
    .expect("fresh admission auto-heal should evaluate");

    assert!(!response.applied);
    assert!(response.skipped);
    assert_eq!(
        response.decision.action,
        RelayRecoveryActionKind::ClearOrphanPendingToken
    );
    assert_eq!(
        response.decision.auto_heal.skipped_reason,
        Some("orphan_token_within_admission_grace")
    );
    assert_eq!(response.decision.evidence.tmux_alive, None);
    assert!(
        response
            .decision
            .evidence
            .mailbox_turn_started_at_ms
            .is_some()
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_some()
    );
    assert!(!token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);

    let manual = auto_apply_relay_recovery_for_shared(
        &registry,
        shared.clone(),
        &provider,
        channel.get(),
        RelayRecoveryActionKind::ClearOrphanPendingToken,
        RelayRecoveryApplySource::Manual,
    )
    .await
    .expect("fresh manual recovery should evaluate");
    assert!(!manual.applied);
    assert_eq!(
        manual.decision.auto_heal.skipped_reason,
        Some("orphan_token_within_admission_grace")
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_some()
    );
    assert!(!token.cancelled.load(Ordering::Relaxed));
}

#[tokio::test]
async fn probe_auto_apply_is_rate_limited_per_channel_action() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(3_360_002);
    start_test_turn(&shared, channel, MessageId::new(92)).await;

    let first = auto_apply_relay_recovery_for_shared_at(
        &registry,
        shared.clone(),
        &provider,
        channel.get(),
        RelayRecoveryActionKind::ClearOrphanPendingToken,
        RelayRecoveryApplySource::ProbeAutoHeal,
        chrono::Utc::now().timestamp_millis()
            + ORPHAN_PENDING_TOKEN_ADMISSION_GRACE.as_millis() as i64,
    )
    .await
    .expect("first orphan token auto-heal should evaluate");
    assert!(first.applied);

    start_test_turn(&shared, channel, MessageId::new(93)).await;
    let second = auto_apply_relay_recovery_for_shared_at(
        &registry,
        shared.clone(),
        &provider,
        channel.get(),
        RelayRecoveryActionKind::ClearOrphanPendingToken,
        RelayRecoveryApplySource::ProbeAutoHeal,
        chrono::Utc::now().timestamp_millis()
            + ORPHAN_PENDING_TOKEN_ADMISSION_GRACE.as_millis() as i64,
    )
    .await
    .expect("second orphan token auto-heal should evaluate");

    assert!(second.skipped);
    assert!(!second.applied);
    assert_eq!(
        second.decision.auto_heal.skipped_reason,
        Some("auto_heal_rate_limited")
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_some(),
        "rate-limited auto-heal must leave the token untouched"
    );
}

#[tokio::test]
async fn watchdog_auto_apply_is_rate_limited_after_first_token_reclaim() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(3_360_005);

    let first_token = start_test_turn(&shared, channel, MessageId::new(94)).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let first = auto_apply_relay_recovery_for_shared_at(
        &registry,
        shared.clone(),
        &provider,
        channel.get(),
        RelayRecoveryActionKind::ClearOrphanPendingToken,
        RelayRecoveryApplySource::StallWatchdog,
        chrono::Utc::now().timestamp_millis()
            + ORPHAN_PENDING_TOKEN_ADMISSION_GRACE.as_millis() as i64,
    )
    .await
    .expect("first watchdog orphan token auto-heal should evaluate");

    assert!(first.applied);
    assert!(!first.skipped);
    assert_eq!(
        first.decision.action,
        RelayRecoveryActionKind::ClearOrphanPendingToken
    );
    assert_eq!(
        first.decision.auto_heal.skipped_reason, None,
        "the first watchdog reclaim in a fresh window must pass"
    );
    assert!(
        first
            .apply_result
            .as_ref()
            .is_some_and(|result| result.removed_mailbox_token)
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_none()
    );
    assert!(first_token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);

    let second_token = start_test_turn(&shared, channel, MessageId::new(95)).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let second = auto_apply_relay_recovery_for_shared_at(
        &registry,
        shared.clone(),
        &provider,
        channel.get(),
        RelayRecoveryActionKind::ClearOrphanPendingToken,
        RelayRecoveryApplySource::StallWatchdog,
        chrono::Utc::now().timestamp_millis()
            + ORPHAN_PENDING_TOKEN_ADMISSION_GRACE.as_millis() as i64,
    )
    .await
    .expect("second watchdog orphan token auto-heal should evaluate");

    assert!(second.skipped);
    assert!(!second.applied);
    assert_eq!(
        second.decision.auto_heal.skipped_reason,
        Some("auto_heal_rate_limited")
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_some(),
        "rate-limited watchdog auto-heal must leave the token untouched"
    );
    assert!(!second_token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn auto_apply_is_limited_to_requested_action_kind() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let parent = ChannelId::new(3_360_003);
    let thread = ChannelId::new(3_360_004);
    shared.dispatch.thread_parents.insert(parent, thread);

    let response = auto_apply_relay_recovery_for_shared(
        &registry,
        shared.clone(),
        &provider,
        parent.get(),
        RelayRecoveryActionKind::ClearOrphanPendingToken,
        RelayRecoveryApplySource::ProbeAutoHeal,
    )
    .await
    .expect("stale thread proof decision should evaluate");

    assert!(response.skipped);
    assert_eq!(
        response.decision.action,
        RelayRecoveryActionKind::ClearStaleThreadProof
    );
    assert_eq!(
        response.decision.auto_heal.skipped_reason,
        Some("auto_heal_action_not_allowed")
    );
    assert!(
        shared.dispatch.thread_parents.contains_key(&parent),
        "auto orphan cleanup must not apply other recovery action kinds"
    );
}

// #3668 F2: the destructive idle-tmux clear must not drop a final answer that
// is still persisted in JSONL after `last_offset`. The guard reads the same
// offset slice via `extract_response_from_output_pub`; when it yields
// non-empty text the caller skips the destructive clear (rebind fall-
// through). When the tail is genuinely empty the guard is silent and the
// existing clear behavior is preserved.
struct AgentdeskRootGuard {
    previous: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}
impl Drop for AgentdeskRootGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

fn write_inflight_with_output(
    provider: &ProviderKind,
    channel_id: u64,
    output_path: &std::path::Path,
    last_offset: u64,
    jsonl_body: &str,
) {
    std::fs::write(output_path, jsonl_body).expect("write output jsonl");
    let state = super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id,
        Some("adk-cdx".to_string()),
        7,
        777,
        7777,
        "hello".to_string(),
        None,
        Some(format!("AgentDesk-codex-adk-cdx-{channel_id}")),
        Some(output_path.to_string_lossy().to_string()),
        None,
        last_offset,
    );
    // full_response stays empty (streaming guard would pass): F2 reproduces
    // the empty-stream + JSONL-terminal-answer asymmetry exactly.
    assert!(state.full_response.is_empty());
    super::super::inflight::save_inflight_state(&state).expect("save inflight");
}

fn set_output_mtime_age(output_path: &std::path::Path, age: std::time::Duration) {
    let modified = std::time::SystemTime::now()
        .checked_sub(age)
        .expect("mtime before now");
    filetime::set_file_mtime(output_path, filetime::FileTime::from_system_time(modified))
        .expect("set output mtime");
}

#[test]
fn frozen_busy_jsonl_uses_ready_pane_fallback_after_stale_window() {
    let _guard = auto_heal_test_lock().blocking_lock();
    let (_root_guard, temp) = isolated_agentdesk_root();

    let provider = ProviderKind::Claude;
    let channel_id = 4_030_501;
    let output_path = temp.path().join("frozen-busy-ready.jsonl");
    let body = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"streaming tail without terminator\"}]}}\n";
    write_inflight_with_output(&provider, channel_id, &output_path, body.len() as u64, body);
    set_output_mtime_age(&output_path, std::time::Duration::from_secs(20 * 60));

    assert!(
        idle_tmux_repair_ready_for_input_with_pane_probe(
            &provider,
            channel_id,
            "tmux-4030-frozen-ready",
            |_tmux, _provider| true,
        ),
        "a long-frozen Busy JSONL may consume the pane-ready fallback"
    );
}

#[test]
fn frozen_busy_jsonl_keeps_deny_when_pane_still_busy() {
    let _guard = auto_heal_test_lock().blocking_lock();
    let (_root_guard, temp) = isolated_agentdesk_root();

    let provider = ProviderKind::Claude;
    let channel_id = 4_030_502;
    let output_path = temp.path().join("frozen-busy-pane-busy.jsonl");
    let body = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"live long-running tool call\"}]}}\n";
    write_inflight_with_output(&provider, channel_id, &output_path, body.len() as u64, body);
    set_output_mtime_age(&output_path, std::time::Duration::from_secs(20 * 60));

    assert!(
        !idle_tmux_repair_ready_for_input_with_pane_probe(
            &provider,
            channel_id,
            "tmux-4030-frozen-busy",
            |_tmux, _provider| false,
        ),
        "a frozen Busy JSONL still denies while the live pane is not ready"
    );
}

#[test]
fn idle_tmux_repair_guard_detects_tail_answer_after_offset() {
    let _guard = auto_heal_test_lock().blocking_lock();
    let (_root_guard, temp) = isolated_agentdesk_root();

    let provider = ProviderKind::Codex;
    let channel_id = 3_668_001;
    let output_path = temp.path().join("out.jsonl");

    // A leading pre-offset record (consumed) followed by a terminal answer
    // record after `last_offset`. `last_offset` points past the first line so
    // only the final answer remains in the extracted slice.
    let pre = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"old\"}]}}\n";
    let post = "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"FINAL ANSWER\"}\n";
    let last_offset = pre.len() as u64;
    write_inflight_with_output(
        &provider,
        channel_id,
        &output_path,
        last_offset,
        &format!("{pre}{post}"),
    );
    let state = super::super::inflight::load_inflight_state(&provider, channel_id)
        .expect("tail-answer guard fixture must save an inflight row");

    assert!(
        idle_tmux_repair_has_unrelayed_tail_answer(&state),
        "JSONL terminal answer after last_offset must block destructive clear"
    );
}

#[test]
fn idle_tmux_repair_guard_silent_when_tail_empty() {
    let _guard = auto_heal_test_lock().blocking_lock();
    let (_root_guard, temp) = isolated_agentdesk_root();

    let provider = ProviderKind::Codex;
    let channel_id = 3_668_002;
    let output_path = temp.path().join("out.jsonl");

    // Only a pre-offset record exists; nothing relayable remains after
    // `last_offset`, so the guard stays silent and the existing destructive
    // clear behavior is preserved (behavior-preserving regression guard).
    let body = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"old\"}]}}\n";
    let last_offset = body.len() as u64;
    write_inflight_with_output(&provider, channel_id, &output_path, last_offset, body);
    let state = super::super::inflight::load_inflight_state(&provider, channel_id)
        .expect("empty-tail guard fixture must save an inflight row");

    assert!(
        !idle_tmux_repair_has_unrelayed_tail_answer(&state),
        "empty JSONL tail must not block the existing destructive clear path"
    );
}

#[test]
fn idle_tmux_repair_guard_silent_when_partial_text_has_no_terminal_result() {
    // #3668 codex r3: a hung/desynced turn that emitted partial assistant
    // text after `last_offset` but NO terminal `result` record must NOT
    // suppress the destructive clear — otherwise the watchdog would skip it
    // every tick forever (recovery only advances the offset on terminal
    // success). The guard requires success-result completion evidence.
    let _guard = auto_heal_test_lock().blocking_lock();
    let (_root_guard, temp) = isolated_agentdesk_root();

    let provider = ProviderKind::Codex;
    let channel_id = 3_668_003;
    let output_path = temp.path().join("out.jsonl");

    let pre = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"old\"}]}}\n";
    let post = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"partial, still streaming...\"}]}}\n";
    let last_offset = pre.len() as u64;
    write_inflight_with_output(
        &provider,
        channel_id,
        &output_path,
        last_offset,
        &format!("{pre}{post}"),
    );
    let state = super::super::inflight::load_inflight_state(&provider, channel_id)
        .expect("partial-tail guard fixture must save an inflight row");

    assert!(
        !idle_tmux_repair_has_unrelayed_tail_answer(&state),
        "partial assistant text without a terminal success result must not block force-clean"
    );
}
