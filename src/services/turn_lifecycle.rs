use poise::serenity_prelude::ChannelId;

use crate::services::discord::health::HealthRegistry;
use crate::services::provider::ProviderKind;
#[cfg(unix)]
use crate::services::tmux_diagnostics::record_tmux_exit_reason;

const DIRECT_FALLBACK_PATH: &str = "direct-fallback";

#[derive(Debug, Clone)]
pub(crate) struct TurnLifecycleTarget {
    pub provider: Option<ProviderKind>,
    pub channel_id: Option<ChannelId>,
    pub tmux_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct TurnLifecycleStopResult {
    pub lifecycle_path: &'static str,
    pub tmux_killed: bool,
    pub inflight_cleared: bool,
    pub queue_depth: Option<usize>,
    pub queue_preserved: bool,
    pub termination_recorded: bool,
}

pub(crate) async fn stop_turn_preserving_queue(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    reason: &str,
) -> TurnLifecycleStopResult {
    stop_turn_preserving_queue_with_cancel_event(health_registry, target, reason, true).await
}

pub(crate) async fn stop_turn_preserving_queue_without_cancel_event(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    reason: &str,
) -> TurnLifecycleStopResult {
    stop_turn_preserving_queue_with_cancel_event(health_registry, target, reason, false).await
}

async fn stop_turn_preserving_queue_with_cancel_event(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    reason: &str,
    emit_cancel_observability: bool,
) -> TurnLifecycleStopResult {
    stop_turn_with_policy(
        health_registry,
        target,
        reason,
        crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight {
            restart_mode: crate::services::discord::InflightRestartMode::HotSwapHandoff,
        },
        emit_cancel_observability,
    )
    .await
}

pub(crate) async fn force_kill_turn(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    reason: &str,
    termination_reason_code: &'static str,
) -> TurnLifecycleStopResult {
    force_kill_turn_with_cancel_event(
        health_registry,
        target,
        reason,
        termination_reason_code,
        true,
    )
    .await
}

pub(crate) async fn force_kill_turn_without_cancel_event(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    reason: &str,
    termination_reason_code: &'static str,
) -> TurnLifecycleStopResult {
    force_kill_turn_with_cancel_event(
        health_registry,
        target,
        reason,
        termination_reason_code,
        false,
    )
    .await
}

async fn force_kill_turn_with_cancel_event(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    reason: &str,
    termination_reason_code: &'static str,
    emit_cancel_observability: bool,
) -> TurnLifecycleStopResult {
    stop_turn_with_policy(
        health_registry,
        target,
        reason,
        crate::services::discord::TmuxCleanupPolicy::CleanupSession {
            termination_reason_code: Some(termination_reason_code),
        },
        emit_cancel_observability,
    )
    .await
}

async fn stop_turn_with_policy(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    reason: &str,
    cleanup_policy: crate::services::discord::TmuxCleanupPolicy,
    emit_cancel_observability: bool,
) -> TurnLifecycleStopResult {
    if let Some(channel_id) = target.channel_id {
        let tmux_session_name = (!target.tmux_name.is_empty()).then_some(target.tmux_name.as_str());
        crate::services::discord::record_turn_stop_tombstone(channel_id, tmux_session_name, reason)
            .await;
    }

    let mut lifecycle_path = DIRECT_FALLBACK_PATH;
    let mut queue_depth = None;
    let mut termination_recorded = false;
    let mut runtime_persistent_inflight_cleared = false;
    let tmux_was_alive = crate::services::platform::tmux::has_session(&target.tmux_name);
    let cleanup_tmux = cleanup_policy.should_cleanup_tmux();

    if let (Some(registry), Some(provider), Some(channel_id)) =
        (health_registry, target.provider.as_ref(), target.channel_id)
    {
        let runtime = if cleanup_tmux {
            let termination_reason_code = match cleanup_policy {
                crate::services::discord::TmuxCleanupPolicy::CleanupSession {
                    termination_reason_code,
                } => termination_reason_code.unwrap_or("force_kill"),
                crate::services::discord::TmuxCleanupPolicy::PreserveSession
                | crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight {
                    ..
                } => "force_kill",
            };
            crate::services::discord::health::force_kill_provider_channel_runtime(
                registry,
                provider.as_str(),
                channel_id,
                reason,
                termination_reason_code,
            )
            .await
        } else {
            crate::services::discord::health::stop_provider_channel_runtime_with_policy(
                registry,
                provider.as_str(),
                channel_id,
                reason,
                cleanup_policy,
            )
            .await
        };
        if let Some(runtime) = runtime {
            lifecycle_path = runtime.lifecycle_path;
            queue_depth = Some(runtime.queue_depth);
            termination_recorded = runtime.termination_recorded;
            runtime_persistent_inflight_cleared = runtime.persistent_inflight_cleared;
        }
    }

    // Some callers only know the tmux session name. When the canonical
    // provider/channel path cannot resolve, fall back to mailbox cleanup by
    // tmux lookup. Force-kill tears down watcher ownership; preserve-session
    // stops clear active-turn state while leaving watcher lifetime to tmux.
    if lifecycle_path == DIRECT_FALLBACK_PATH
        && let Some(registry) = health_registry
    {
        let hard_stop = if cleanup_tmux {
            crate::services::discord::health::hard_stop_runtime_turn(
                Some(registry),
                target.provider.as_ref().map(|provider| provider.as_str()),
                target.channel_id.map(|channel_id| channel_id.get()),
                Some(&target.tmux_name),
                "turn_lifecycle_direct_fallback",
            )
            .await
        } else {
            crate::services::discord::health::stop_runtime_turn_preserving_watcher(
                Some(registry),
                target.provider.as_ref().map(|provider| provider.as_str()),
                target.channel_id.map(|channel_id| channel_id.get()),
                Some(&target.tmux_name),
                "turn_lifecycle_preserve_direct_fallback",
            )
            .await
        };
        if hard_stop.cleanup_path != "runtime_unavailable_fallback" {
            lifecycle_path = hard_stop.cleanup_path;
        }
    }

    let tmux_killed = if cleanup_tmux {
        #[cfg(unix)]
        if crate::services::platform::tmux::has_session(&target.tmux_name) {
            record_tmux_exit_reason(&target.tmux_name, &format!("explicit cleanup via {reason}"));
        }

        let killed_now = if crate::services::platform::tmux::has_session(&target.tmux_name) {
            crate::services::platform::tmux::kill_session_with_reason(
                &target.tmux_name,
                &format!("explicit cleanup via {reason}"),
            )
        } else {
            tmux_was_alive
        };
        // Delete persistent + legacy session temp files alongside the kill
        // so /tmp and ~/.adk/release/runtime/sessions/ don't accumulate
        // stale jsonl/FIFO/owner markers after forced termination (#892).
        if killed_now {
            crate::services::tmux_common::cleanup_session_temp_files(&target.tmux_name);
        }
        killed_now
    } else {
        false
    };

    let inflight_cleared = if cleanup_policy.should_clear_inflight() {
        target.provider.as_ref().is_some_and(|provider| {
            let cleared_by_tmux = clear_inflight_by_tmux_name(provider, &target.tmux_name);
            let cleared_by_channel = target
                .channel_id
                .is_some_and(|channel_id| clear_inflight_by_channel(provider, channel_id));

            runtime_persistent_inflight_cleared || cleared_by_tmux || cleared_by_channel
        })
    } else {
        false
    };

    let result = TurnLifecycleStopResult {
        lifecycle_path,
        tmux_killed,
        inflight_cleared,
        queue_depth,
        queue_preserved: true,
        termination_recorded,
    };

    if emit_cancel_observability && should_emit_cancel_observability(target, &result) {
        crate::services::turn_cancel_finalizer::finalize_turn_cancel(
            crate::services::turn_cancel_finalizer::FinalizeTurnCancelRequest::from_lifecycle_result(
                crate::services::turn_cancel_finalizer::TurnCancelCorrelation {
                    provider: target.provider.clone(),
                    channel_id: target.channel_id,
                    dispatch_id: None,
                    session_key: None,
                    turn_id: None,
                },
                reason,
                cleanup_policy_observability_surface(cleanup_policy),
                &result,
            )
        );
    }

    result
}

fn should_emit_cancel_observability(
    target: &TurnLifecycleTarget,
    result: &TurnLifecycleStopResult,
) -> bool {
    target.channel_id.is_some()
        || result.lifecycle_path != DIRECT_FALLBACK_PATH
        || result.tmux_killed
        || result.inflight_cleared
        || result.queue_depth.is_some()
        || result.termination_recorded
}

pub(crate) fn cleanup_policy_observability_surface(
    cleanup_policy: crate::services::discord::TmuxCleanupPolicy,
) -> &'static str {
    match cleanup_policy {
        crate::services::discord::TmuxCleanupPolicy::PreserveSession => "preserve_session_cancel",
        crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight { .. } => {
            "queue_cancel_preserve"
        }
        crate::services::discord::TmuxCleanupPolicy::CleanupSession { .. } => "force_kill_cancel",
    }
}

#[cfg(test)]
mod policy_observability_tests {
    use crate::services::discord::{InflightRestartMode, TmuxCleanupPolicy};

    #[test]
    fn cleanup_policy_observability_surface_matches_cancel_contract() {
        assert_eq!(
            super::cleanup_policy_observability_surface(TmuxCleanupPolicy::PreserveSession),
            "preserve_session_cancel"
        );
        assert_eq!(
            super::cleanup_policy_observability_surface(
                TmuxCleanupPolicy::PreserveSessionAndInflight {
                    restart_mode: InflightRestartMode::HotSwapHandoff,
                },
            ),
            "queue_cancel_preserve"
        );
        assert_eq!(
            super::cleanup_policy_observability_surface(TmuxCleanupPolicy::CleanupSession {
                termination_reason_code: Some("force_kill"),
            }),
            "force_kill_cancel"
        );
    }

    #[test]
    fn cancel_observability_skips_only_unknown_noop_fallback() {
        let target = super::TurnLifecycleTarget {
            provider: None,
            channel_id: None,
            tmux_name: "missing-session".to_string(),
        };
        let noop = super::TurnLifecycleStopResult {
            lifecycle_path: super::DIRECT_FALLBACK_PATH,
            tmux_killed: false,
            inflight_cleared: false,
            queue_depth: None,
            queue_preserved: true,
            termination_recorded: false,
        };
        assert!(!super::should_emit_cancel_observability(&target, &noop));

        let mut mailbox_cleanup = noop.clone();
        mailbox_cleanup.lifecycle_path = "mailbox_canonical";
        assert!(super::should_emit_cancel_observability(
            &target,
            &mailbox_cleanup
        ));

        let channel_scoped_target = super::TurnLifecycleTarget {
            provider: None,
            channel_id: Some(poise::serenity_prelude::ChannelId::new(42)),
            tmux_name: "missing-session".to_string(),
        };
        assert!(super::should_emit_cancel_observability(
            &channel_scoped_target,
            &noop
        ));
    }
}

/// Scan inflight directory for the provider and delete the file matching the
/// given tmux session.
///
/// Thin wrapper that delegates to the single-owner implementation in
/// `services::discord::inflight` (see `docs/recovery-paths.md` — inflight
/// cleanup SSoT, issue #1074). Kept as a function rather than inlined so that
/// existing call sites in this module continue to read naturally.
pub(crate) fn clear_inflight_by_tmux_name(provider: &ProviderKind, tmux_name: &str) -> bool {
    crate::services::discord::clear_inflight_by_tmux_name(provider, tmux_name)
}

fn clear_inflight_by_channel(provider: &ProviderKind, channel_id: ChannelId) -> bool {
    crate::services::discord::clear_inflight_state(provider, channel_id.get())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use crate::services::discord::health::TestHealthHarness;
    use crate::services::provider::ProviderKind;

    // #1074: the turn_lifecycle wrapper must delegate to the discord SSoT
    // rather than re-implement the inflight directory scan. Hitting the
    // wrapper with a tmux name that cannot exist still returns `false`
    // cleanly — no panic from an unresolved code path.
    #[test]
    fn clear_inflight_by_tmux_name_delegates_to_discord_ssot() {
        let result = super::clear_inflight_by_tmux_name(
            &ProviderKind::Codex,
            "AgentDesk-codex-ssot-probe-1074-lifecycle-cdx",
        );
        assert!(
            !result,
            "turn_lifecycle wrapper must delegate cleanly and return false for unknown tmux name"
        );
    }

    #[test]
    fn preserve_session_handoff_policy_keeps_inflight_metadata() {
        assert!(
            !crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight {
                restart_mode: crate::services::discord::InflightRestartMode::HotSwapHandoff,
            }
            .should_clear_inflight()
        );
        assert!(
            crate::services::discord::TmuxCleanupPolicy::PreserveSession.should_clear_inflight()
        );
        assert!(
            crate::services::discord::TmuxCleanupPolicy::CleanupSession {
                termination_reason_code: None,
            }
            .should_clear_inflight()
        );
    }

    // #964: queue-api `cancel_turn` emits `killed=false` when the
    // `PreserveSessionAndInflight` policy is used. The DoD item #3 pins the
    // invariant that the watcher registry slot MUST survive such a cancel —
    // only force-kill (cleanup_tmux=true) is allowed to tear down the watcher.
    #[tokio::test]
    async fn stop_turn_preserving_queue_with_killed_false_does_not_cancel_watcher() {
        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let channel_id = 223_456_789_012_345_679;
        let channel_name = "watcher-preserve-cancel";
        let tmux_name = ProviderKind::Codex.build_tmux_session_name(channel_name);

        harness
            .seed_channel_session(channel_id, channel_name, Some("session-preserve"))
            .await;
        harness.seed_active_turn(channel_id, 55, 66).await;
        let cancel_flag = harness.seed_watcher(channel_id);

        let registry = harness.registry();
        let result = super::stop_turn_preserving_queue(
            Some(registry.as_ref()),
            &super::TurnLifecycleTarget {
                provider: Some(ProviderKind::Codex),
                channel_id: Some(poise::serenity_prelude::ChannelId::new(channel_id)),
                tmux_name,
            },
            "queue-api cancel_turn (killed=false preservation test)",
        )
        .await;

        // PreserveSessionAndInflight + no actual tmux session → tmux_killed=false.
        assert!(
            !result.tmux_killed,
            "preserve policy must report killed=false"
        );
        assert!(!result.inflight_cleared);
        assert!(result.queue_preserved);

        // Critical DoD invariant: watcher registry slot survives the cancel
        // and the stale cancel flag was NOT flipped. A future re-dispatch can
        // reuse or replace this watcher without racing a silent teardown.
        assert!(
            harness.has_watcher(channel_id),
            "watcher registry entry must be preserved across killed=false cancel",
        );
        assert!(
            !cancel_flag.load(std::sync::atomic::Ordering::Relaxed),
            "watcher cancel flag must NOT be set on killed=false cancel",
        );
    }

    #[tokio::test]
    async fn direct_fallback_force_kill_clears_mailbox_by_tmux_lookup() {
        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let channel_id = 223_456_789_012_345_678;
        let channel_name = "fallback-mailbox-cleanup";
        let tmux_name = ProviderKind::Codex.build_tmux_session_name(channel_name);

        harness
            .seed_channel_session(channel_id, channel_name, Some("session-fallback"))
            .await;
        harness.seed_active_turn(channel_id, 55, 66).await;
        harness
            .seed_queue(channel_id, &[(9_001, "preserve queued follow-up")])
            .await;

        let registry = harness.registry();
        let result = super::force_kill_turn(
            Some(registry.as_ref()),
            &super::TurnLifecycleTarget {
                provider: Some(ProviderKind::Codex),
                channel_id: None,
                tmux_name,
            },
            "test direct fallback",
            "force_kill",
        )
        .await;

        assert_eq!(result.lifecycle_path, "mailbox_canonical");
        assert!(!result.tmux_killed);
        assert!(!result.inflight_cleared);
        assert!(result.queue_preserved);

        let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(channel_id).await;
        assert!(!has_active_turn);
        assert_eq!(queue_depth, 1);
        assert_eq!(session_id, None);
    }

    #[tokio::test]
    async fn preserve_session_direct_fallback_does_not_cancel_watcher_by_tmux_lookup() {
        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let channel_id = 223_456_789_012_345_680;
        let channel_name = "preserve-fallback-watcher";
        let tmux_name = ProviderKind::Codex.build_tmux_session_name(channel_name);

        harness
            .seed_channel_session(channel_id, channel_name, Some("session-preserve-fallback"))
            .await;
        harness.seed_active_turn(channel_id, 55, 66).await;
        let watcher_cancel = harness.seed_watcher(channel_id);

        let registry = harness.registry();
        let result = super::stop_turn_preserving_queue(
            Some(registry.as_ref()),
            &super::TurnLifecycleTarget {
                provider: Some(ProviderKind::Codex),
                channel_id: None,
                tmux_name,
            },
            "preserve fallback must not infer tmux death",
        )
        .await;

        assert_eq!(result.lifecycle_path, "mailbox_canonical");
        assert!(!result.tmux_killed);
        assert!(!result.inflight_cleared);

        let (has_active_turn, _, _) = harness.mailbox_state(channel_id).await;
        assert!(
            !has_active_turn,
            "preserve-session fallback must clear active mailbox state",
        );
        assert!(
            harness.has_watcher(channel_id),
            "preserve-session fallback must leave live watcher ownership attached",
        );
        assert!(
            !watcher_cancel.load(std::sync::atomic::Ordering::Relaxed),
            "preserve-session fallback must not cancel the watcher",
        );
    }

    #[tokio::test]
    async fn force_kill_direct_fallback_still_cancels_watcher_by_tmux_lookup() {
        let harness = TestHealthHarness::new_with_provider(ProviderKind::Codex).await;
        let channel_id = 223_456_789_012_345_681;
        let channel_name = "force-kill-fallback-watcher";
        let tmux_name = ProviderKind::Codex.build_tmux_session_name(channel_name);

        harness
            .seed_channel_session(channel_id, channel_name, Some("session-force-fallback"))
            .await;
        harness.seed_active_turn(channel_id, 55, 66).await;
        let watcher_cancel = harness.seed_watcher(channel_id);

        let registry = harness.registry();
        let result = super::force_kill_turn(
            Some(registry.as_ref()),
            &super::TurnLifecycleTarget {
                provider: Some(ProviderKind::Codex),
                channel_id: None,
                tmux_name,
            },
            "force kill fallback should detach watcher",
            "force_kill",
        )
        .await;

        assert_eq!(result.lifecycle_path, "mailbox_canonical");
        assert!(
            !harness.has_watcher(channel_id),
            "force-kill fallback should remove watcher ownership",
        );
        assert!(
            watcher_cancel.load(std::sync::atomic::Ordering::Relaxed),
            "force-kill fallback should cancel the watcher",
        );
    }
}
