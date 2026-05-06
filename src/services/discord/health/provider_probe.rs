use std::collections::HashMap;

use poise::serenity_prelude::ChannelId;
use serde::Serialize;

use super::{HealthStatus, ProviderEntry};
use crate::services::turn_orchestrator::ChannelMailboxSnapshot;

#[derive(Debug, Serialize)]
pub(super) struct ProviderHealthSnapshot {
    name: String,
    connected: bool,
    active_turns: usize,
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
        .restart_pending
        .load(std::sync::atomic::Ordering::Relaxed);
    let connected = entry
        .shared
        .bot_connected
        .load(std::sync::atomic::Ordering::Relaxed);
    let reconcile_done = entry
        .shared
        .reconcile_done
        .load(std::sync::atomic::Ordering::Relaxed);
    let deferred_hooks = entry
        .shared
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
    signals: ProviderProbeSignals,
) -> ProviderHealthClassification {
    let mut status = HealthStatus::Healthy;
    let mut fully_recovered = true;
    let mut degraded_reasons = Vec::new();

    if !signals.connected {
        status = status.worsen(HealthStatus::Unhealthy);
        degraded_reasons.push(format!("provider:{provider_name}:disconnected"));
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
        .recovery_duration_ms
        .load(std::sync::atomic::Ordering::Relaxed);
    let duration_ms = if recorded_ms > 0 {
        recorded_ms
    } else {
        let elapsed_ms = shared.recovery_started_at.elapsed().as_millis();
        elapsed_ms.min(u64::MAX as u128) as u64
    };
    duration_ms as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::{ProviderProbeSignals, classify_provider};
    use crate::services::discord::health::HealthStatus;

    #[test]
    fn classifier_preserves_healthy_provider() {
        let result = classify_provider(
            "codex",
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
}
