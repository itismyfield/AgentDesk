use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use poise::serenity_prelude::ChannelId;
use serde::Serialize;

use super::{HealthStatus, ProviderEntry, ProviderRuntimeRole};
use crate::services::turn_orchestrator::ChannelMailboxSnapshot;

#[derive(Debug, Serialize)]
pub(super) struct ProviderHealthSnapshot {
    name: String,
    runtime_role: ProviderRuntimeRole,
    connected: bool,
    active_turns: usize,
    runtime_state_complete: bool,
    queue_depth: usize,
    sessions: usize,
    restart_pending: bool,
    last_turn_at: Option<String>,
    /// #5951 — per-channel re-mint fence cells; never pruned, so this only
    /// grows with the distinct channels the runtime has served.
    remint_fence_cells: usize,
    /// Boot writer channels this node leaves to the gateway; present only when there are some.
    #[serde(skip_serializing_if = "Option::is_none")]
    tui_output_gateway_channels: Option<usize>,
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
    /// How long this runtime has been up, measured from the same anchor
    /// `recovery_duration_secs` uses. Only consulted while `reconcile_done` is
    /// false, to bound how long the block stays anonymous (#5449).
    reconcile_age: Duration,
    deferred_hooks: usize,
    queue_depth: usize,
    recovering_channels: usize,
    /// This node leaves some of the provider's writer channels to the gateway, cannot tell, or
    /// refuses the provider's intake outright.
    tui_output_requires_gateway: bool,
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
    let reconcile_age = entry.shared.restart.recovery_started_at.elapsed();
    if reconcile_stalled(reconcile_done, reconcile_age) {
        warn_reconcile_stall_once(&entry.name, entry.role, reconcile_age);
    }
    let recovery_duration = recovery_duration_secs(&entry.shared);
    let last_turn_at = entry
        .shared
        .last_turn_at
        .lock()
        .ok()
        .and_then(|g| g.clone());
    let (gateway_channels, intake_refused) = tui_output_gateway(&entry.name, entry.role);

    let classification = classify_provider(
        &entry.name,
        entry.role,
        ProviderProbeSignals {
            connected,
            restart_pending,
            reconcile_done,
            reconcile_age,
            deferred_hooks,
            queue_depth,
            recovering_channels,
            tui_output_requires_gateway: intake_refused || gateway_channels != Some(0),
        },
    );

    ProviderProbe {
        mailbox_snapshots,
        snapshot: ProviderHealthSnapshot {
            name: entry.name.clone(),
            runtime_role: entry.role,
            connected,
            active_turns,
            runtime_state_complete: true,
            queue_depth,
            sessions: session_count,
            restart_pending,
            last_turn_at,
            remint_fence_cells: entry.shared.mailboxes.remint_fence_cells(),
            tui_output_gateway_channels: gateway_channels.filter(|count| *count > 0),
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
    if signals.tui_output_requires_gateway {
        status = status.worsen(HealthStatus::Degraded);
        degraded_reasons.push(format!(
            "provider:{provider_name}:{}",
            crate::services::tui_o::topology::TUI_OUTPUT_REQUIRES_GATEWAY
        ));
    }
    if signals.restart_pending {
        status = status.worsen(HealthStatus::Unhealthy);
        degraded_reasons.push(format!("provider:{provider_name}:restart_pending"));
    }
    if !signals.reconcile_done {
        // The promotion renames a finite obligation; the `status` and
        // `fully_recovered` effects are identical to `reconcile_in_progress`, so
        // no reader of those two axes changes polarity. The reason STRING is what
        // discriminates the two: the deploy readiness gate tolerates
        // `reconcile_in_progress` and denies `reconcile_stalled`
        // (`_defaults.sh::_health_json_has_reconcile_stalled`, #5071 S0 r2 F3).
        status = status.worsen(HealthStatus::Degraded);
        let reason = if reconcile_stalled(signals.reconcile_done, signals.reconcile_age) {
            "reconcile_stalled"
        } else {
            "reconcile_in_progress"
        };
        degraded_reasons.push(format!("provider:{provider_name}:{reason}"));
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

/// Writer channels this entry's role leaves to the gateway (`None` when the boot list is unknown),
/// and whether the intake worker gate still refuses the whole provider here.
fn tui_output_gateway(provider_name: &str, role: ProviderRuntimeRole) -> (Option<usize>, bool) {
    use crate::services::tui_o::channel_policy::BootChannels;
    use crate::services::tui_o::topology::{self, HostRole};
    let role = match role {
        ProviderRuntimeRole::Gateway => HostRole::Gateway,
        ProviderRuntimeRole::Standby => HostRole::Standby,
        ProviderRuntimeRole::Worker => HostRole::Runner,
    };
    let facts = |enabled: bool, boot: Option<&BootChannels>| {
        let channels = topology::gateway_only_channels(enabled, provider_name, role, boot);
        (
            channels,
            !topology::intake_worker_allowed(enabled, provider_name, role),
        )
    };
    #[cfg(test)]
    {
        use crate::services::tui_o::cutover::test_override;
        let enabled = topology::O_TUI_WRITER || test_override::forced();
        test_override::with_channels(|boot| facts(enabled, boot))
    }
    #[cfg(not(test))]
    facts(
        topology::O_TUI_WRITER,
        crate::services::tui_o::channel_policy::boot(),
    )
}

/// Whether an unfinished reconcile has outlived its boot-relative deadline and
/// must be reported as stalled rather than in-progress (#5449). One predicate so
/// the reason string and the WARN cannot disagree about the promotion.
fn reconcile_stalled(reconcile_done: bool, reconcile_age: Duration) -> bool {
    !reconcile_done && reconcile_age >= super::RECONCILE_STALL_AFTER
}

/// Providers already WARNed about a stalled reconcile in this process. The
/// promotion is re-derived on every health poll, so the diagnostic is latched per
/// provider name instead of repeating once per poll.
static RECONCILE_STALL_WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Name a stalled reconcile once per provider per process. The registry is not
/// reachable from a per-provider probe, so this reports the entry's own name and
/// role plus the one process-wide fact that explains the common cause — whether
/// this boot's startup doctor found an empty provider registry.
fn warn_reconcile_stall_once(provider_name: &str, role: ProviderRuntimeRole, age: Duration) {
    let Ok(mut warned) = RECONCILE_STALL_WARNED.lock() else {
        return;
    };
    if !warned.insert(provider_name.to_string()) {
        return;
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ provider {provider_name} reconcile unfinished after {}s (role={role:?}, startup_doctor_saw_empty_registry={}) — reported as reconcile_stalled; deploys stay blocked",
        age.as_secs(),
        super::startup_doctor_saw_empty_registry()
    );
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

    use std::time::Duration;

    use super::{ProviderProbeSignals, classify_provider};
    use crate::services::discord::health::{
        HealthRegistry, HealthStatus, ProviderRuntimeRole, RECONCILE_STALL_AFTER,
        build_health_snapshot,
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
                reconcile_age: Duration::ZERO,
                deferred_hooks: 0,
                queue_depth: 0,
                recovering_channels: 0,
                tui_output_requires_gateway: false,
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
                reconcile_age: Duration::ZERO,
                deferred_hooks: 2,
                queue_depth: 3,
                recovering_channels: 1,
                tui_output_requires_gateway: false,
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
        // TUI-O writer alarms are process-global and raised by concurrent tests.
        let reasons: Vec<&str> = json["degraded_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|reason| reason.as_str())
            .filter(|reason| !reason.starts_with("tui_o:"))
            .collect();
        assert_eq!(reasons, ["provider:codex:gateway_standby"]);
    }

    /// #5951 — the fence cell of a purged channel stays, and health says so.
    #[tokio::test]
    async fn provider_health_counts_fence_cells_that_outlive_their_actor() {
        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        registry.register("codex".to_string(), shared.clone()).await;
        let channel = ChannelId::new(NEXT_STANDBY_TEST_CHANNEL.fetch_add(1, Ordering::Relaxed));
        let _ = shared.mailbox(channel);
        assert_eq!(
            shared.mailboxes.remove_idle_entry(channel).await,
            crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome::Removed
        );

        let json = serde_json::to_value(build_health_snapshot(&registry).await).unwrap();
        assert_eq!(json["providers"][0]["remint_fence_cells"], 1);
    }

    #[tokio::test]
    async fn worker_profile_health_does_not_require_gateway_or_hide_recovery_failure() {
        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        registry
            .register_worker("codex".to_string(), shared.clone())
            .await;
        assert!(!registry.all_providers_are_standby().await);
        let snapshot = build_health_snapshot(&registry).await;
        assert_eq!(snapshot.status(), HealthStatus::Healthy);
        let json = serde_json::to_value(snapshot).unwrap();
        assert_eq!(json["providers"][0]["runtime_role"], "worker");
        assert_eq!(json["providers"][0]["connected"], false);
        shared
            .restart
            .restart_pending
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let snapshot = build_health_snapshot(&registry).await;
        assert_eq!(snapshot.status(), HealthStatus::Unhealthy);
        assert!(
            serde_json::to_value(snapshot).unwrap()["degraded_reasons"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("provider:codex:restart_pending"))
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
                reconcile_age: Duration::ZERO,
                deferred_hooks: 2,
                queue_depth: 3,
                recovering_channels: 1,
                tui_output_requires_gateway: false,
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

    #[test]
    fn refused_tui_intake_is_a_degraded_reason_on_every_non_gateway_role() {
        for (role, expected) in [
            (
                ProviderRuntimeRole::Standby,
                &[
                    "provider:claude:gateway_standby",
                    "provider:claude:tui_output_requires_gateway",
                ][..],
            ),
            (
                ProviderRuntimeRole::Worker,
                &["provider:claude:tui_output_requires_gateway"][..],
            ),
        ] {
            let result = classify_provider(
                "claude",
                role,
                ProviderProbeSignals {
                    tui_output_requires_gateway: true,
                    ..reconcile_pending_signals(Duration::ZERO)
                },
            );
            assert_eq!(result.status, HealthStatus::Degraded, "{role:?}");
            assert_eq!(
                &result.degraded_reasons[..expected.len()],
                expected,
                "{role:?}"
            );
        }
    }

    fn reconcile_pending_signals(reconcile_age: Duration) -> ProviderProbeSignals {
        ProviderProbeSignals {
            connected: true,
            restart_pending: false,
            reconcile_done: false,
            reconcile_age,
            deferred_hooks: 0,
            queue_depth: 0,
            recovering_channels: 0,
            tui_output_requires_gateway: false,
        }
    }

    // T-S0-3: an unfinished reconcile is a finite obligation — once it outlives
    // the threshold it must stop being reported as merely in progress.
    #[test]
    fn unfinished_reconcile_is_promoted_to_stalled_after_the_threshold() {
        let stalled = classify_provider(
            "codex",
            ProviderRuntimeRole::Gateway,
            reconcile_pending_signals(RECONCILE_STALL_AFTER),
        );

        assert_eq!(
            stalled.degraded_reasons,
            ["provider:codex:reconcile_stalled"]
        );
    }

    // The promotion is a rename, not an admission: it keeps the exact status and
    // `fully_recovered` effect the pre-threshold reason had. Scope note (#5071 S0
    // r2 F3): this covers the two HEALTH axes only — the deploy gate deliberately
    // does not treat the two reasons alike, and that difference lives in
    // `_defaults.sh`, not here.
    #[test]
    fn stalled_reconcile_keeps_the_in_progress_health_axes() {
        let in_progress = classify_provider(
            "codex",
            ProviderRuntimeRole::Gateway,
            reconcile_pending_signals(RECONCILE_STALL_AFTER - Duration::from_secs(1)),
        );
        let stalled = classify_provider(
            "codex",
            ProviderRuntimeRole::Gateway,
            reconcile_pending_signals(RECONCILE_STALL_AFTER),
        );

        assert_eq!(
            in_progress.degraded_reasons,
            ["provider:codex:reconcile_in_progress"]
        );
        assert_eq!(in_progress.status, HealthStatus::Degraded);
        assert_eq!(stalled.status, in_progress.status);
        assert_eq!(stalled.fully_recovered, in_progress.fully_recovered);
        assert!(!stalled.fully_recovered);
    }
}
