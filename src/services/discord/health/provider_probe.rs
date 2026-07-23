use std::collections::HashMap;

use poise::serenity_prelude::ChannelId;
use serde::Serialize;

use super::{HealthStatus, ProviderEntry, ProviderRuntimeRole};
use crate::services::turn_orchestrator::ChannelMailboxSnapshot;

#[derive(Debug, Serialize)]
pub(super) struct ProviderHealthSnapshot {
    name: String,
    connected: bool,
    active_turns: usize,
    runtime_state_complete: bool,
    queue_depth: usize,
    sessions: usize,
    restart_pending: bool,
    last_turn_at: Option<String>,
}

pub(super) struct ProviderProbe {
    pub(super) mailbox_snapshots: HashMap<ChannelId, ChannelMailboxSnapshot>,
    pub(super) snapshot: ProviderHealthSnapshot,
    pub(super) status: HealthStatus,
    pub(super) fully_recovered: bool,
    pub(super) deferred_hooks: usize,
    pub(super) queue_depth: usize,
    pub(super) watcher_count: usize,
    pub(super) recovery_duration: f64,
    pub(super) degraded_reasons: Vec<String>,
}

struct ProviderProbeSignals {
    connected: bool,
    restart_pending: bool,
    reconcile_done: bool,
    deferred_hooks: usize,
    queue_depth: usize,
    recovering_channels: usize,
}

struct ProviderHealthClassification {
    status: HealthStatus,
    fully_recovered: bool,
    degraded_reasons: Vec<String>,
}

pub(super) async fn probe_provider(entry: &ProviderEntry) -> ProviderProbe {
    let session_count = entry
        .shared
        .core
        .try_lock()
        .map(|data| data.sessions.len())
        .unwrap_or(0);
    let mailbox_snapshots = entry.shared.mailboxes.snapshot_all().await;
    let active_turns = mailbox_snapshots
        .values()
        .filter(|snapshot| snapshot.cancel_token.is_some())
        .count();
    let queue_depth: usize = mailbox_snapshots
        .values()
        .map(|snapshot| snapshot.intervention_queue.len())
        .sum();

    let restart_pending = entry
        .shared
        .restart
        .restart_pending
        .load(std::sync::atomic::Ordering::Relaxed);
    let connected = entry
        .shared
        .bot_connected
        .load(std::sync::atomic::Ordering::Relaxed);
    let reconcile_done = entry
        .shared
        .restart
        .reconcile_done
        .load(std::sync::atomic::Ordering::Relaxed);
    let deferred_hooks = entry
        .shared
        .restart
        .deferred_hook_backlog
        .load(std::sync::atomic::Ordering::Relaxed);
    let watcher_count = entry.shared.tmux_watchers.len();
    let recovering_channels = mailbox_snapshots
        .values()
        .filter(|snapshot| snapshot.recovery_started_at.is_some())
        .count();
    let recovery_duration = recovery_duration_secs(&entry.shared);
    let last_turn_at = entry
        .shared
        .last_turn_at
        .lock()
        .ok()
        .and_then(|g| g.clone());

    let classification = classify_provider(
        &entry.name,
        entry.role,
        ProviderProbeSignals {
            connected,
            restart_pending,
            reconcile_done,
            deferred_hooks,
            queue_depth,
            recovering_channels,
        },
    );

    ProviderProbe {
        mailbox_snapshots,
        snapshot: ProviderHealthSnapshot {
            name: entry.name.clone(),
            connected,
            active_turns,
            runtime_state_complete: true,
            queue_depth,
            sessions: session_count,
            restart_pending,
            last_turn_at,
        },
        status: classification.status,
        fully_recovered: classification.fully_recovered,
        deferred_hooks,
        queue_depth,
        watcher_count,
        recovery_duration,
        degraded_reasons: classification.degraded_reasons,
    }
}

fn classify_provider(
    provider_name: &str,
    role: ProviderRuntimeRole,
    signals: ProviderProbeSignals,
) -> ProviderHealthClassification {
    let mut status = HealthStatus::Healthy;
    let mut fully_recovered = true;
    let mut degraded_reasons = Vec::new();

    if role.requires_gateway_connection() && !signals.connected {
        status = status.worsen(HealthStatus::Unhealthy);
        degraded_reasons.push(format!("provider:{provider_name}:disconnected"));
    } else if role == ProviderRuntimeRole::Standby {
        status = status.worsen(HealthStatus::Degraded);
        degraded_reasons.push(format!("provider:{provider_name}:gateway_standby"));
    }
    if signals.restart_pending {
        status = status.worsen(HealthStatus::Unhealthy);
        degraded_reasons.push(format!("provider:{provider_name}:restart_pending"));
    }
    if !signals.reconcile_done {
        status = status.worsen(HealthStatus::Degraded);
        degraded_reasons.push(format!("provider:{provider_name}:reconcile_in_progress"));
        fully_recovered = false;
    }
    if signals.deferred_hooks > 0 {
        status = status.worsen(HealthStatus::Degraded);
        degraded_reasons.push(format!(
            "provider:{provider_name}:deferred_hooks_backlog:{}",
            signals.deferred_hooks
        ));
    }
    if signals.queue_depth > 0 {
        status = status.worsen(HealthStatus::Degraded);
        degraded_reasons.push(format!(
            "provider:{provider_name}:pending_queue_depth:{}",
            signals.queue_depth
        ));
    }
    if signals.recovering_channels > 0 {
        status = status.worsen(HealthStatus::Degraded);
        degraded_reasons.push(format!(
            "provider:{provider_name}:recovering_channels:{}",
            signals.recovering_channels
        ));
        fully_recovered = false;
    }

    ProviderHealthClassification {
        status,
        fully_recovered,
        degraded_reasons,
    }
}

fn recovery_duration_secs(shared: &super::super::SharedData) -> f64 {
    let recorded_ms = shared
        .restart
        .recovery_duration_ms
        .load(std::sync::atomic::Ordering::Relaxed);
    let duration_ms = if recorded_ms > 0 {
        recorded_ms
    } else {
        let elapsed_ms = shared.restart.recovery_started_at.elapsed().as_millis();
        elapsed_ms.min(u64::MAX as u128) as u64
    };
    duration_ms as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    use super::{ProviderProbeSignals, classify_provider};
    use crate::services::discord::health::{
        HealthRegistry, HealthStatus, ProviderRuntimeRole, build_health_snapshot,
    };
    use crate::services::provider::CancelToken;

    static NEXT_STANDBY_TEST_CHANNEL: AtomicU64 = AtomicU64::new(9_700_000_000);

    #[test]
    fn classifier_preserves_healthy_provider() {
        let result = classify_provider(
            "codex",
            ProviderRuntimeRole::Gateway,
            ProviderProbeSignals {
                connected: true,
                restart_pending: false,
                reconcile_done: true,
                deferred_hooks: 0,
                queue_depth: 0,
                recovering_channels: 0,
            },
        );

        assert_eq!(result.status, HealthStatus::Healthy);
        assert!(result.fully_recovered);
        assert!(result.degraded_reasons.is_empty());
    }

    #[test]
    fn classifier_reports_probe_degradation_reasons() {
        let result = classify_provider(
            "codex",
            ProviderRuntimeRole::Gateway,
            ProviderProbeSignals {
                connected: false,
                restart_pending: true,
                reconcile_done: false,
                deferred_hooks: 2,
                queue_depth: 3,
                recovering_channels: 1,
            },
        );

        assert_eq!(result.status, HealthStatus::Unhealthy);
        assert!(!result.fully_recovered);
        assert_eq!(
            result.degraded_reasons,
            [
                "provider:codex:disconnected",
                "provider:codex:restart_pending",
                "provider:codex:reconcile_in_progress",
                "provider:codex:deferred_hooks_backlog:2",
                "provider:codex:pending_queue_depth:3",
                "provider:codex:recovering_channels:1",
            ]
        );
    }

    #[tokio::test]
    async fn registered_idle_standby_is_degraded_but_http_ready() {
        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        registry.register_standby("codex".to_string(), shared).await;

        let snapshot = build_health_snapshot(&registry).await;

        assert_eq!(snapshot.status(), HealthStatus::Degraded);
        assert!(snapshot.status().is_http_ready());
        assert!(registry.all_providers_are_standby().await);
        let json = serde_json::to_value(snapshot).expect("serialize standby health");
        assert_eq!(json["providers"][0]["connected"], false);
        assert_eq!(json["providers"][0]["runtime_state_complete"], true);
        assert_eq!(
            json["degraded_reasons"],
            serde_json::json!(["provider:codex:gateway_standby"])
        );
    }

    #[tokio::test]
    async fn standby_live_and_finalizing_evidence_remains_visible_for_fail_closed_restart() {
        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        registry
            .register_standby("codex".to_string(), shared.clone())
            .await;

        let channel = ChannelId::new(NEXT_STANDBY_TEST_CHANNEL.fetch_add(1, Ordering::Relaxed));
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                shared.as_ref(),
                channel,
                Arc::new(CancelToken::new()),
                UserId::new(1),
                MessageId::new(2),
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);
        shared.restart.global_finalizing.store(1, Ordering::Relaxed);

        let json = serde_json::to_value(build_health_snapshot(&registry).await)
            .expect("serialize active standby health");

        assert_eq!(json["global_active"], 1);
        assert_eq!(json["global_finalizing"], 1);
        assert_eq!(json["providers"][0]["active_turns"], 1);
        assert_eq!(json["providers"][0]["runtime_state_complete"], true);
        assert_eq!(json["mailboxes"][0]["has_cancel_token"], true);
    }

    #[tokio::test]
    async fn mixed_gateway_and_standby_is_not_classified_as_cluster_standby() {
        let registry = HealthRegistry::new();
        registry
            .register(
                "claude".to_string(),
                crate::services::discord::make_shared_data_for_tests(),
            )
            .await;
        registry
            .register_standby(
                "codex".to_string(),
                crate::services::discord::make_shared_data_for_tests(),
            )
            .await;

        assert!(!registry.all_providers_are_standby().await);
    }

    #[test]
    fn standby_role_only_ignores_gateway_disconnect_and_masks_no_other_axis() {
        let result = classify_provider(
            "codex",
            ProviderRuntimeRole::Standby,
            ProviderProbeSignals {
                connected: false,
                restart_pending: true,
                reconcile_done: false,
                deferred_hooks: 2,
                queue_depth: 3,
                recovering_channels: 1,
            },
        );

        assert_eq!(result.status, HealthStatus::Unhealthy);
        assert!(!result.fully_recovered);
        assert_eq!(
            result.degraded_reasons,
            [
                "provider:codex:gateway_standby",
                "provider:codex:restart_pending",
                "provider:codex:reconcile_in_progress",
                "provider:codex:deferred_hooks_backlog:2",
                "provider:codex:pending_queue_depth:3",
                "provider:codex:recovering_channels:1",
            ]
        );
    }
}
