//! Execute committed takeover/restore intents through the canonical runtime lifecycle.
use crate::services::agent_recovery::{
    self, ChannelRecoveryStatus, DetectorSignal, ObserveInput, OperationPlan, PendingOperation,
};
use crate::services::discord::health::{self, HealthRegistry};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
use serde_json::json;

/// A removed owner watcher must not make its pending recovery undiscoverable.
pub(super) async fn reconcile_provider(
    registry: &HealthRegistry,
    provider: &ProviderKind,
) -> std::collections::HashSet<u64> {
    let mut channels = std::collections::HashSet::new();
    for channel in agent_recovery::active_channels(provider) {
        let Ok(id) = channel.parse::<u64>() else {
            continue;
        };
        channels.insert(id);
        if let Some(snapshot) = registry
            .snapshot_watcher_state_for_provider(provider, id)
            .await
        {
            observe_and_execute(registry, &snapshot).await;
        }
    }
    channels
}

pub(in crate::services::discord) async fn observe_and_execute(
    registry: &HealthRegistry,
    snapshot: &super::WatcherStateSnapshot,
) -> bool {
    let health = &snapshot.relay_health;
    let channel = health.channel_id.to_string();
    let Some(owner) = agent_recovery::owner_provider(&channel) else {
        return false;
    };
    if owner.as_str() != health.provider {
        return false;
    }
    let _execution = match agent_recovery::try_execution(&channel).await {
        Ok(Some(guard)) => guard,
        Ok(None) => return true,
        Err(error) => {
            tracing::warn!(channel, error = %error, "recovery executor unavailable");
            return true;
        }
    };
    let state = match agent_recovery::recovery_state(&channel).await {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(channel, error = %error, "recovery ownership unavailable");
            return true;
        }
    };
    match state.as_ref().map(|state| state.status) {
        Some(ChannelRecoveryStatus::FallbackDone) => {
            // The old owner tmux was deliberately killed. Its registered runtime
            // can now start fresh from the checkpoint, even without that tmux.
            agent_recovery::try_restore_owner_durable(&channel, &owner, true, false).await;
        }
        Some(ChannelRecoveryStatus::FallbackRunning) => {
            if let Some(fallback) = agent_recovery::fallback_provider(&channel)
                && let Some(snapshot) = registry
                    .snapshot_watcher_state_for_provider(&fallback, health.channel_id)
                    .await
                && !snapshot.relay_health.mailbox_has_cancel_token
                && crate::services::discord::inflight::load_inflight_state_read_only(
                    &fallback,
                    health.channel_id,
                )
                .is_none()
            {
                // Also recovers a lost completion write with an idle warm process.
                if let Err(error) = agent_recovery::retry_interrupted_durable(&channel).await {
                    tracing::warn!(channel, error = %error, "fallback retry intent could not commit");
                }
            }
        }
        Some(ChannelRecoveryStatus::TakeoverPending | ChannelRecoveryStatus::RestorePending) => {}
        _ => {
            let turn = health
                .mailbox_active_user_msg_id
                .unwrap_or(health.channel_id)
                .to_string();
            let checkpoint = crate::services::discord::inflight::load_inflight_state_read_only(
                &owner,
                health.channel_id,
            )
            .map(|state| {
                agent_recovery::CheckpointPayload::compact(
                    "",
                    state.user_text.chars().take(4000).collect::<String>(),
                    state.full_response.chars().take(8000).collect::<String>(),
                    "",
                    Vec::new(),
                    "Inspect the inherited workspace and continue the unfinished request.",
                    state.user_text,
                )
            });
            let workspace = if let Some(shared) = registry
                .shared_for_provider_on_channel(&owner, ChannelId::new(health.channel_id))
                .await
            {
                shared
                    .core
                    .lock()
                    .await
                    .sessions
                    .get(&ChannelId::new(health.channel_id))
                    .and_then(|session| session.current_path.clone())
            } else {
                None
            };
            let outcome = agent_recovery::observe_with_checkpoint_durable(
                ObserveInput {
                    channel_id: channel.clone(),
                    primary_turn_id: turn.clone(),
                    signal: DetectorSignal::Mailbox {
                        kind: agent_recovery::mailbox_kind_from_name(
                            snapshot.relay_stall_state.as_str(),
                        ),
                        elapsed_secs: health
                            .mailbox_turn_age_secs
                            .unwrap_or(0)
                            .min(u64::from(u32::MAX)) as u32,
                        claimed_turn: health.mailbox_has_cancel_token,
                    },
                },
                checkpoint.clone(),
                workspace.clone(),
            )
            .await;
            if outcome.spawn.is_none()
                && health.tmux_alive == Some(false)
                && health.mailbox_has_cancel_token
            {
                agent_recovery::observe_with_checkpoint_durable(
                    ObserveInput {
                        channel_id: channel.clone(),
                        primary_turn_id: turn,
                        signal: DetectorSignal::TmuxSessionDead,
                    },
                    checkpoint,
                    workspace,
                )
                .await;
            }
        }
    }
    match agent_recovery::pending_operation(&channel).await {
        Ok(Some(operation)) => {
            execute_operation(registry, operation).await;
            true
        }
        Ok(None) => state.is_some_and(|state| state.lock_held()),
        Err(error) => {
            tracing::warn!(channel, error = %error, "pending recovery work unavailable");
            true
        }
    }
}

async fn fence_runtime(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    channel: ChannelId,
) -> bool {
    let session = registry
        .snapshot_watcher_state_for_provider(provider, channel.get())
        .await
        .and_then(|snapshot| snapshot.tmux_session);
    let process_backend = session
        .as_deref()
        .is_some_and(|name| crate::services::session_backend::process_session_pid(name).is_some());
    let stopped = health::force_kill_provider_channel_runtime(
        registry,
        provider.as_str(),
        channel,
        "recovery pending intent reconciliation",
        "agent_recovery_pending_reconcile",
    )
    .await
    .is_some_and(|result| result.mailbox_foreground_free);
    if !stopped {
        return false;
    }
    let Some(session) = session else {
        return true;
    };
    if process_backend {
        return !crate::services::session_backend::process_session_is_alive(&session);
    }
    // Keep the pre-cleanup identity; registry removal alone is not proof of death.
    tokio::task::spawn_blocking(move || {
        matches!(
            crate::services::platform::tmux::pane_liveness(&session),
            crate::services::platform::tmux::PaneLiveness::DeadOrAbsent
        )
    })
    .await
    .unwrap_or(false)
}

async fn execute_operation(registry: &HealthRegistry, operation: PendingOperation) {
    let Ok(id) = operation.lease.channel_id.parse::<u64>() else {
        return;
    };
    let channel = ChannelId::new(id);
    // A crash may have happened immediately after launch, before acknowledgement.
    if !fence_runtime(registry, &operation.owner_provider, channel).await
        || !fence_runtime(registry, &operation.fallback_provider, channel).await
    {
        tracing::warn!(
            channel_id = id,
            "recovery remains pending: both runtimes must be fenced"
        );
        return;
    }
    let (provider, prompt, metadata) = match operation.plan {
        OperationPlan::Fallback(plan) => (
            operation.fallback_provider,
            plan.prompt,
            recovery_metadata(
                "agent-recovery-fallback",
                &plan.fallback_agent_id,
                "fresh",
                "fallback",
            ),
        ),
        OperationPlan::Restore(plan) => (
            operation.owner_provider,
            plan.packet,
            recovery_metadata(
                "agent-recovery-restore",
                &plan.owner_agent_id,
                "fresh",
                "restore",
            ),
        ),
    };
    let start = agent_recovery::admission::with_recovery_start(
        operation.lease.clone(),
        health::start_headless_agent_turn(
            registry,
            channel,
            provider,
            prompt,
            Some("agent-recovery".into()),
            Some(metadata),
            None,
        ),
    )
    .await;
    match start {
        Ok(outcome) => {
            if let Err(error) = agent_recovery::acknowledge_start_durable(&operation.lease).await {
                tracing::warn!(channel_id = id, error = %error, "launch acknowledgement failed; pending intent will reconcile");
            } else {
                tracing::info!(channel_id = id, turn_id = %outcome.turn_id, "recovery launch acknowledged");
            }
        }
        Err(error) => {
            tracing::warn!(channel_id = id, error = %error, "recovery launch failed; retaining pending intent");
            if let Err(error) =
                agent_recovery::retry_interrupted_durable(&operation.lease.channel_id).await
            {
                tracing::warn!(channel_id = id, error = %error, "failed to fence the interrupted start");
            }
        }
    }
}
fn recovery_metadata(
    routine_id: &str,
    agent_id: &str,
    execution_strategy: &str,
    recovery_mode: &str,
) -> serde_json::Value {
    json!({ "routine_id": routine_id, "agent_id": agent_id, "execution_strategy": execution_strategy, "agent_recovery": { "mode": recovery_mode } })
}
#[cfg(test)]
mod tests {
    use super::recovery_metadata;
    #[test]
    fn recovery_metadata_is_a_role_bound_persistent_routine_when_requested() {
        let metadata =
            recovery_metadata("agent-recovery-restore", "claude", "persistent", "restore");
        assert_eq!(metadata["routine_id"], "agent-recovery-restore");
        assert_eq!(metadata["agent_id"], "claude");
        assert_eq!(metadata["execution_strategy"], "persistent");
        assert_eq!(metadata["agent_recovery"]["mode"], "restore");
    }
}
