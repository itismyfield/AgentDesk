//! Relay recovery under the input fence: a closed gate skips before reserving or mutating, an
//! unprotected channel applies as before.

use super::super::auto_heal_attempts::auto_heal_attempt_counters_for_tests;
use super::*;
use crate::services::discord::input_runtime::{self, fence::Gate};

struct PendingApplyBoundary;

#[async_trait::async_trait]
impl super::super::auto_heal_apply::ReservedEpisodeApplyBoundary for PendingApplyBoundary {
    async fn after_reserve(&self, _episode: &circuit_breaker::RelayReattachEpisode) {
        std::future::pending::<()>().await;
    }
}

#[tokio::test]
async fn c2_admitted_relay_apply_holds_input_drain_through_reserved_episode() {
    use futures::FutureExt;
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = 6_325_534;
    let state = super::super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel,
        Some("relay-fence".into()),
        7,
        channel + 10,
        channel + 20,
        "input".into(),
        None,
        None,
        None,
        None,
        0,
    );
    super::super::super::inflight::save_inflight_state_create_new(&state).unwrap();
    let mut health = snapshot();
    health.channel_id = channel;
    let mut decision = plan_relay_recovery(
        &health,
        RelayStallState::TmuxAliveRelayDead,
        chrono::Utc::now().timestamp_millis(),
    );
    decision.action = RelayRecoveryActionKind::ReattachWatcher;
    decision.auto_heal.eligible = true;
    decision.auto_heal.skipped_reason = None;
    decision.affected.channel_id = channel;
    decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());
    decision.affected.mailbox_active_user_msg_id = Some(state.user_msg_id);
    decision.affected.tmux_session = None;
    let gate = Gate::protect(provider.clone(), channel).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    let mut apply = Box::pin(
        super::super::auto_heal_apply::apply_relay_recovery_plan_with_seams(
            &registry,
            &shared,
            &provider,
            decision,
            chrono::Utc::now().timestamp_millis(),
            RelayRecoveryApplySource::StallWatchdog,
            &circuit_breaker::PgCircuitAlertEnqueue,
            &PendingApplyBoundary,
        ),
    );
    assert!(
        futures::poll!(apply.as_mut()).is_pending(),
        "apply must reach reservation boundary"
    );
    let key = auto_heal_key(
        "codex",
        channel,
        RelayRecoveryActionKind::ReattachWatcher,
        RelayRecoveryApplySource::StallWatchdog,
    );
    assert!(auto_heal_attempt_counters_for_tests(&key).is_some());
    let closing = gate.close().unwrap();
    assert!(
        closing.drain().now_or_never().is_none(),
        "reserved apply lost its permit"
    );
    drop(apply);
    assert!(
        closing.drain().now_or_never().is_some(),
        "cancelled apply leaked its permit"
    );
}

#[tokio::test]
async fn c2_closed_input_gate_skips_relay_recovery_before_reserving_or_mutating() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let action = RelayRecoveryActionKind::ClearStaleThreadProof;
    let source = RelayRecoveryApplySource::ProbeAutoHeal;
    let (open, fenced) = (ChannelId::new(6_325_530), ChannelId::new(6_325_532));
    shared
        .dispatch
        .thread_parents
        .insert(open, ChannelId::new(6_325_531));
    shared
        .dispatch
        .thread_parents
        .insert(fenced, ChannelId::new(6_325_533));
    let gate = Gate::protect(provider.clone(), fenced.get()).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    let _closing = gate.close().unwrap();
    let apply = |channel: ChannelId| {
        auto_apply_relay_recovery_for_shared(
            &registry,
            shared.clone(),
            &provider,
            channel.get(),
            action,
            source,
        )
    };

    let applied = apply(open).await.expect("open channel evaluates");
    assert!(applied.applied, "{:?}", applied.decision.auto_heal);
    assert!(!shared.dispatch.thread_parents.contains_key(&open));

    let skipped = apply(fenced).await.expect("fenced channel evaluates");
    assert!(skipped.skipped && !skipped.applied);
    assert_eq!(skipped.decision.action, action);
    assert_eq!(
        skipped.decision.auto_heal.skipped_reason,
        Some("input_fenced")
    );
    assert!(skipped.apply_result.is_none());
    let mut direct_decision = skipped.decision.clone();
    direct_decision.auto_heal.eligible = true;
    direct_decision.auto_heal.skipped_reason = None;
    let direct = super::super::auto_heal_apply::apply_relay_recovery_plan_with_seams(
        &registry,
        &shared,
        &provider,
        direct_decision,
        chrono::Utc::now().timestamp_millis(),
        source,
        &circuit_breaker::PgCircuitAlertEnqueue,
        &PendingApplyBoundary,
    )
    .await;
    assert!(direct.skipped && !direct.applied);
    assert_eq!(
        direct.decision.auto_heal.skipped_reason,
        Some("input_fenced")
    );
    assert!(
        shared.dispatch.thread_parents.contains_key(&fenced),
        "proof kept"
    );
    let key = auto_heal_key("codex", fenced.get(), action, source);
    assert!(
        auto_heal_attempt_counters_for_tests(&key).is_none(),
        "nothing reserved"
    );
    assert!(
        input_runtime::health_reasons()
            .iter()
            .any(|reason| reason.contains(&format!("channel={}", fenced.get())))
    );
}

struct GapApplyBoundary(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);

#[async_trait::async_trait]
impl super::super::auto_heal_apply::ReservedEpisodeApplyBoundary for GapApplyBoundary {
    async fn after_reserve(&self, _episode: &circuit_breaker::RelayReattachEpisode) {
        self.0.notify_one();
        self.1.notified().await;
    }
}

/// An admitted automatic reattach on a protected open channel completes its pinned rebind; a gate
/// closed after the reservation waits for the apply, whose return releases the drain.
#[cfg(unix)]
#[tokio::test]
async fn c2b_admitted_relay_reattach_rebinds_through_a_closing_gate_and_releases_the_drain() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::inflight;
    use crate::services::session_host::test_support::InjectedLivenessGuard;
    use crate::services::session_host::{HostLiveness, HostSessionRef};
    use futures::FutureExt;
    use std::os::unix::fs::PermissionsExt;
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let (_root_guard, root_dir) = isolated_agentdesk_root();
    // A `tmux` that reports every pane alive and lists no sessions.
    let tmux_dir = tempfile::tempdir().unwrap();
    let script = tmux_dir.path().join("tmux");
    std::fs::write(
        &script,
        "#!/bin/sh\nwhile [ \"${1#-}\" != \"$1\" ]; do shift; done\ncase \"$1\" in list-panes) echo 0 ;; esac; exit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _path =
        crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(tmux_dir.path());
    let provider = ProviderKind::Claude;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    registry
        .register_http(
            provider.as_str().to_string(),
            Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
        )
        .await;
    let channel = 6_325_535_u64;
    let tmux = format!("AgentDesk-claude-c2ra{channel}-{}-cc", std::process::id());
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        &tmux,
        RuntimeHandoffKind::ClaudeTui,
    )
    .unwrap();
    let session = format!("48fdb7f3-6325-4000-8000-{channel:012}");
    let transcript = root_dir.path().join(format!("{session}.jsonl"));
    std::fs::write(&transcript, vec![b'x'; 4_096]).unwrap();
    let mut orphan = inflight::InflightTurnState::new(
        provider.clone(),
        channel,
        None,
        0,
        0,
        channel + 7,
        String::new(),
        Some(session),
        Some(tmux.clone()),
        Some(transcript.display().to_string()),
        None,
        4_096,
    );
    orphan.turn_source = inflight::TurnSource::ExternalInput;
    orphan.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
    assert!(inflight::save_inflight_state_if_absent(&orphan).unwrap());
    let state = inflight::load_inflight_state_read_only(&provider, channel).unwrap();
    let row = input_runtime::fence::population_root()
        .unwrap()
        .join("discord_inflight/claude")
        .join(format!("{channel}.json"));
    let before = std::fs::read(&row).unwrap();
    let _live = InjectedLivenessGuard::set(HostSessionRef::tmux(&tmux), HostLiveness::Live);
    let mut health = snapshot();
    health.provider = provider.as_str().to_string();
    health.channel_id = channel;
    let mut decision = plan_relay_recovery(
        &health,
        RelayStallState::TmuxAliveRelayDead,
        chrono::Utc::now().timestamp_millis(),
    );
    decision.action = RelayRecoveryActionKind::ReattachWatcher;
    decision.auto_heal.eligible = true;
    decision.auto_heal.skipped_reason = None;
    decision.affected.channel_id = channel;
    decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());
    decision.affected.tmux_session = Some(tmux.clone());
    let gate = Gate::protect(provider.clone(), channel).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    let (reached, resume) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let boundary = GapApplyBoundary(reached.clone(), resume.clone());

    let apply = super::super::auto_heal_apply::apply_relay_recovery_plan_with_seams(
        &registry,
        &shared,
        &provider,
        decision,
        chrono::Utc::now().timestamp_millis(),
        RelayRecoveryApplySource::StallWatchdog,
        &circuit_breaker::PgCircuitAlertEnqueue,
        &boundary,
    );
    tokio::pin!(apply);
    tokio::select! {
        _ = &mut apply => panic!("the apply must reserve before it rebinds"),
        _ = reached.notified() => {}
    }
    let closing = gate.close().unwrap();
    assert!(
        closing.drain().now_or_never().is_none(),
        "the reserved apply holds the drain"
    );
    resume.notify_one();
    let response = apply.await;

    let watcher = shared
        .tmux_watchers
        .remove(&ChannelId::new(channel))
        .map(|(_, watcher)| watcher);
    if let Some(watcher) = watcher.as_ref() {
        watcher.cancel.store(true, Ordering::Relaxed);
    }
    let result = response.apply_result.as_ref().expect("apply result");
    assert_eq!(result.reattach_watcher_spawned, Some(true), "{response:?}");
    assert!(watcher.is_some(), "the apply rebound a watcher");
    assert_ne!(
        std::fs::read(&row).unwrap(),
        before,
        "the rebind rewrote its row"
    );
    assert!(
        !input_runtime::health_reasons()
            .iter()
            .any(|reason| reason.contains(&format!("channel={channel}"))),
        "the admitted apply saw a refused writer"
    );
    assert!(
        closing.drain().now_or_never().is_some(),
        "returning releases the drain"
    );
}
