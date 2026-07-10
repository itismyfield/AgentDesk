use std::sync::Arc;

use super::auto_heal_attempts::{
    auto_heal_key, commit_auto_heal_attempt, record_auto_heal_confirm_failure,
    refund_auto_heal_attempt, release_auto_heal_attempt, remaining_auto_heal_attempts,
    reserve_auto_heal_attempt,
};
use super::auto_heal_confirm::{ReattachConfirmation, classify_reattach_confirmation};
use super::*;

pub(super) async fn apply_relay_recovery_plan(
    registry: &HealthRegistry,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    mut decision: RelayRecoveryDecision,
    now_ms: i64,
    source: RelayRecoveryApplySource,
) -> RelayRecoveryResponse {
    if !decision.auto_heal.eligible {
        trace_relay_recovery_skipped(&decision, decision.auto_heal.skipped_reason);
        return RelayRecoveryResponse {
            ok: false,
            mode: "apply",
            applied: false,
            skipped: true,
            decision,
            apply_result: None,
        };
    }

    let key = auto_heal_key(
        &decision.provider,
        decision.channel_id,
        decision.action,
        source,
    );
    decision.auto_heal.remaining_attempts =
        remaining_auto_heal_attempts(&key, now_ms, decision.auto_heal.max_attempts_per_window);
    match reserve_auto_heal_attempt(&key, now_ms, decision.auto_heal.max_attempts_per_window) {
        Ok(remaining) => decision.auto_heal.remaining_attempts = remaining,
        Err(reason) => {
            decision.auto_heal.remaining_attempts = 0;
            decision.auto_heal.skipped_reason = Some(reason);
            trace_relay_recovery_skipped(&decision, Some(reason));
            return RelayRecoveryResponse {
                ok: false,
                mode: "apply",
                applied: false,
                skipped: true,
                decision,
                apply_result: None,
            };
        }
    }

    let mut apply_result =
        apply_relay_recovery_decision(registry, shared, provider, &decision, source).await;
    let confirmation = classify_reattach_confirmation(
        shared,
        &decision,
        &apply_result,
        registry.started_at_unix(),
        chrono::Utc::now().timestamp(),
    )
    .await;
    match confirmation {
        ReattachConfirmation::StartupGrace => {
            apply_result.status = "reattach_confirm_startup_grace";
            release_auto_heal_attempt(&key);
        }
        ReattachConfirmation::Failed => {
            apply_result.status = "reattach_confirm_failed";
            apply_result.reattach_error = Some(
                "spawned watcher did not confirm heartbeat or relay-frontier progress".to_string(),
            );
            record_auto_heal_confirm_failure(&key, now_ms);
        }
        ReattachConfirmation::NotRequired | ReattachConfirmation::Confirmed => {
            if matches!(
                apply_result.status,
                "rebind_failed" | "provider_unavailable"
            ) {
                refund_auto_heal_attempt(&key, now_ms);
            } else {
                commit_auto_heal_attempt(&key);
            }
        }
    }
    decision.auto_heal.remaining_attempts =
        remaining_auto_heal_attempts(&key, now_ms, decision.auto_heal.max_attempts_per_window);
    let applied = relay_recovery_status_counts_as_applied(apply_result.status);
    tracing::info!(
        target: "agentdesk::discord::relay_recovery",
        provider = decision.provider.as_str(),
        channel_id = decision.channel_id,
        action = decision.action.as_str(),
        source = source.as_str(),
        status = apply_result.status,
        confirmation = ?confirmation,
        remaining_attempts = decision.auto_heal.remaining_attempts,
        removed_thread_proofs = apply_result.removed_thread_proofs,
        removed_mailbox_token = apply_result.removed_mailbox_token,
        "relay recovery auto-heal attempt completed"
    );
    RelayRecoveryResponse {
        ok: applied,
        mode: "apply",
        applied,
        skipped: false,
        decision,
        apply_result: Some(apply_result),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    use super::super::auto_heal_attempts::{
        auto_heal_test_lock, clear_auto_heal_attempts_for_tests,
    };
    use super::*;
    use crate::services::provider::CancelToken;

    async fn start_turn(shared: &Arc<SharedData>, channel: ChannelId, message: u64) {
        let token = Arc::new(CancelToken::new());
        assert!(
            super::super::super::mailbox_try_start_turn(
                shared,
                channel,
                token,
                UserId::new(1),
                MessageId::new(message),
            )
            .await
        );
    }

    #[test]
    fn relay_recovery_manual_apply_succeeds_after_probe_budget_is_exhausted() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        // Match the established suite-wide lock order: auto-heal budget first,
        // shared environment second. The inverse order deadlocks parallel tests.
        let _guard = runtime.block_on(auto_heal_test_lock().lock());
        let root = tempfile::tempdir().expect("isolated AgentDesk root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        runtime.block_on(async {
            clear_auto_heal_attempts_for_tests();
            let provider = ProviderKind::Codex;
            let registry = HealthRegistry::new();
            let shared = super::super::super::make_shared_data_for_tests();
            registry
                .register(provider.as_str().to_string(), shared.clone())
                .await;
            let channel = ChannelId::new(4_423_301);

            start_turn(&shared, channel, 4_423_311).await;
            let first = auto_apply_relay_recovery_for_shared(
                &registry,
                shared.clone(),
                &provider,
                channel.get(),
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                RelayRecoveryApplySource::ProbeAutoHeal,
            )
            .await
            .expect("first probe apply");
            assert!(first.applied);

            start_turn(&shared, channel, 4_423_312).await;
            let blocked_probe = auto_apply_relay_recovery_for_shared(
                &registry,
                shared.clone(),
                &provider,
                channel.get(),
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                RelayRecoveryApplySource::ProbeAutoHeal,
            )
            .await
            .expect("exhausted probe apply");
            assert!(blocked_probe.skipped);
            assert_eq!(
                blocked_probe.decision.auto_heal.skipped_reason,
                Some("auto_heal_rate_limited")
            );

            let manual = auto_apply_relay_recovery_for_shared(
                &registry,
                shared,
                &provider,
                channel.get(),
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                RelayRecoveryApplySource::Manual,
            )
            .await
            .expect("manual apply after probe exhaustion");
            assert!(
                manual.applied,
                "internal probe budget must never lock an operator"
            );
        });
    }
}
