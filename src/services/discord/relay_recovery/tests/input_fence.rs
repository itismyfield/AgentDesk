//! Relay recovery under the input fence: a closed gate skips before reserving or mutating, an
//! unprotected channel applies as before.

use super::super::auto_heal_attempts::auto_heal_attempt_counters_for_tests;
use super::*;
use crate::services::discord::input_runtime::{self, fence::Gate};

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
