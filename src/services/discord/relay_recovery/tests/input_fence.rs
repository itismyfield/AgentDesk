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
