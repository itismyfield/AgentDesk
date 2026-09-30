//! Execute committed takeover/restore intents through the canonical runtime lifecycle.
use crate::services::agent_recovery::{
    self, ChannelRecoveryStatus, DetectorSignal, ObserveInput, OperationPlan, PendingOperation,
};
use crate::services::discord::health::{self, HealthRegistry};
use crate::services::discord::host_teardown_gate::{ChannelTeardown, channel_teardown};
use crate::services::provider::ProviderKind;
use crate::services::session_host::legacy_collapse::dead_only_if_dead_or_absent;
use crate::services::session_host::{HostKind, HostSessionRef, host_for};
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
    // Only a found legacy row may be force-killed; any other answer keeps the pending intent.
    if let Some(name) = session.as_deref() {
        let gate = channel_teardown(registry, provider, channel, name, None, "recovery_fence");
        if !matches!(gate.await, ChannelTeardown::Cleared(_)) {
            return false;
        }
    }
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
        dead_only_if_dead_or_absent(
            host_for(HostKind::Tmux).liveness(HostSessionRef::tmux(&session)),
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

#[cfg(test)]
mod host_guard_tests {
    use crate::services::agent_recovery::{
        self, ChannelRecoveryStatus, ChannelState, CheckpointPayload, DetectorSignal, ObserveInput,
        OrgAgentInput, OrgChannelInput, RecoveryCatalog, RecoveryConfigWire, RecoveryLease,
        build_recovery_catalog, test_store,
    };
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, busy_turn, channel_key, runtime, seed, turn_kept,
    };
    use crate::services::provider::ProviderKind;
    use crate::services::session_host::test_support::InjectedLivenessGuard;
    use crate::services::session_host::{HostLiveness, HostSessionRef};
    use poise::serenity_prelude::ChannelId;

    fn recovery() -> RecoveryConfigWire {
        RecoveryConfigWire {
            enabled: Some(true),
            fallback_agent_id: Some("monitoring".to_string()),
            stall_secs: Some(180),
            workspace_mode: Some("inherit".to_string()),
            triggers: None,
        }
    }

    /// Claude owns every channel and falls back to codex.
    fn catalog(channels: &[ChannelId]) -> RecoveryCatalog {
        let agent = |id: &str, provider: &str, recovery| OrgAgentInput {
            id: id.to_string(),
            provider: Some(provider.to_string()),
            model: None,
            workspace: Some(format!("/p4a-{id}")),
            auth_profile: "default".into(),
            recovery,
        };
        let channels: Vec<_> = channels
            .iter()
            .map(|channel| OrgChannelInput {
                channel_id: channel.get().to_string(),
                agent: "claude".to_string(),
                provider: None,
                workspace: None,
                auth_profile: None,
                recovery: Some(recovery()),
            })
            .collect();
        let agents = [
            agent("claude", "claude", Some(recovery())),
            agent("monitoring", "codex", None),
        ];
        build_recovery_catalog(&agents, &channels).expect("recovery catalog")
    }

    /// Commits a pending takeover, or a pending restore after the fallback finished.
    async fn commit_pending(channel: &str, restore: bool) -> ChannelState {
        let state = || async { agent_recovery::recovery_state(channel).await.unwrap() };
        let observe = ObserveInput {
            channel_id: channel.to_string(),
            primary_turn_id: format!("{channel}-turn"),
            signal: DetectorSignal::StreamIdleTimeout,
        };
        assert!(
            agent_recovery::observe_durable(observe)
                .await
                .spawn
                .is_some()
        );
        if restore {
            let lease = RecoveryLease::from_state(&state().await.unwrap());
            agent_recovery::acknowledge_start_durable(&lease)
                .await
                .unwrap();
            let done = CheckpointPayload::compact("", "", "done", "", Vec::new(), "", "");
            agent_recovery::complete_turn_durable(&lease, done)
                .await
                .unwrap();
            let claude = ProviderKind::Claude;
            let plan = agent_recovery::try_restore_owner_durable(channel, &claude, true, false);
            assert!(plan.await.is_some(), "{channel}");
        }
        let pending = state().await.unwrap();
        let expected = match restore {
            true => ChannelRecoveryStatus::RestorePending,
            false => ChannelRecoveryStatus::TakeoverPending,
        };
        assert_eq!(pending.status, expected, "{channel}");
        pending
    }

    // A committed takeover or restore through the executor: a refused fence leaves the durable
    // intent as it was, and a legacy fence holds only on a confirmed dead pane.
    #[tokio::test]
    async fn recovery_fence_stops_only_a_found_legacy_row_and_keeps_the_intent_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (shared, registry) = runtime(&pool).await;
        let claude = ProviderKind::Claude;
        let legacy = [HostLiveness::DeadOrAbsent, HostLiveness::ProbeError]
            .map(|liveness| (Stored::Legacy, liveness));
        let refused = Stored::ALL[1..]
            .iter()
            .map(|stored| (*stored, HostLiveness::DeadOrAbsent));
        let cases: Vec<_> = legacy
            .into_iter()
            .chain(refused)
            .flat_map(|case| [(case, false), (case, true)])
            .collect();
        let channels: Vec<_> = (0..cases.len() as u64)
            .map(|n| ChannelId::new(1_479_671_301_387_059_700 + n))
            .collect();
        let executed = async {
            for (((stored, liveness), restore), channel) in cases.into_iter().zip(&channels) {
                let channel = *channel;
                let name = format!("AgentDesk-claude-p4a-fence-{}", channel.get());
                let key = channel_key(&shared, &name);
                seed(&pool, &key, &name, channel.get(), stored).await;
                let token = busy_turn(&shared, channel, &name).await;
                let id = channel.get().to_string();
                let before = commit_pending(&id, restore).await;
                let _pane = InjectedLivenessGuard::set(HostSessionRef::tmux(&name), liveness);
                let case = format!("{stored:?} {liveness:?} restore={restore}");
                let snapshot = registry
                    .snapshot_watcher_state_for_provider(&claude, channel.get())
                    .await
                    .expect("watcher snapshot");
                if stored == Stored::Legacy {
                    let fenced = super::fence_runtime(&registry, &claude, channel).await;
                    let dead = liveness == HostLiveness::DeadOrAbsent;
                    assert_eq!(fenced, dead, "{case}");
                    assert!(!turn_kept(&shared, channel, &token).await, "{case}");
                }
                assert!(
                    super::observe_and_execute(&registry, &snapshot).await,
                    "{case}"
                );
                let after = agent_recovery::recovery_state(&id).await.unwrap();
                assert_eq!(after.as_ref(), Some(&before), "{case}");
                if stored != Stored::Legacy {
                    assert!(turn_kept(&shared, channel, &token).await, "{case}");
                    let queued = crate::services::discord::mailbox_snapshot(&shared, channel);
                    assert!(queued.await.intervention_queue.is_empty(), "{case}");
                }
            }
        };
        test_store::with_store(pool.clone(), catalog(&channels), executed).await;
        pool.close().await;
        db.drop().await;
    }
}
