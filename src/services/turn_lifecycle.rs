use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;
use crate::services::discord::health::{
    FINISH_UNOBSERVED_PATH, HealthRegistry, TOKEN_SUPERSEDED_PATH,
};
use crate::services::discord::host_teardown_gate::{
    ChannelTeardown, backfill_inflight_after_guard, guard_tmux_name, nameless_runtime_teardown,
    row_gate, runtime_target_holds, runtime_teardown,
};
use crate::services::provider::ProviderKind;
#[cfg(unix)]
use crate::services::tmux_diagnostics::record_tmux_exit_reason;

const DIRECT_FALLBACK_PATH: &str = "direct-fallback";
const HOST_GUARD_KEPT_PATH: &str = "host-guard-kept";

#[cfg(test)]
static FORCE_KILL_PRESERVE_TMUX_FOR_TESTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
pub(crate) fn set_force_kill_preserve_tmux_for_test(tmux_session_name: &str, preserve: bool) {
    let mut sessions = FORCE_KILL_PRESERVE_TMUX_FOR_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if preserve {
        sessions.insert(tmux_session_name.to_string());
    } else {
        sessions.remove(tmux_session_name);
    }
}

#[cfg(test)]
fn force_kill_preserves_tmux_for_test(tmux_session_name: &str) -> bool {
    FORCE_KILL_PRESERVE_TMUX_FOR_TESTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(tmux_session_name)
}

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
    /// Measured queue preservation; absent if either memory or disk could not be observed.
    pub queue_preserved: Option<bool>,
    pub termination_recorded: bool,
    /// #1672: best-effort tmux session name resolved at cancel time.
    /// Populated even when the caller passed an empty `tmux_name`, by
    /// looking up the watcher binding / inflight state / channel session
    /// before the cancel runs. Used by the cancel API response so
    /// operators can no longer see `tmux_session: ""` while the runtime
    /// knows perfectly well which session is being stopped.
    pub tmux_session_observed: Option<String>,
    /// #1672: in-memory mailbox queue depth captured *before* the cancel
    /// ran (`None` when the registry had no shared runtime for this
    /// provider/channel pair).
    pub queue_depth_before: Option<usize>,
    /// #1672: same as `queue_depth_before` but captured after the cancel
    /// completed. Drives the post-fact `queue_preserved` invariant.
    pub queue_depth_after: Option<usize>,
    /// File presence immediately before cancel; absent when unmeasured.
    pub queue_disk_present_before: Option<bool>,
    /// File presence after cancel; only measured true-to-false proves disk loss.
    pub queue_disk_present_after: Option<bool>,
    /// #5176: whether the channel mailbox actually gave up its foreground
    /// turn anchor. THIS is what "the turn was cancelled" has to mean — a
    /// `turn_status: cancelled` stamp on a mailbox that still owns the
    /// foreground slot leaves the channel permanently unusable, which is the
    /// exact incident this field exists to make visible in the API response.
    /// `None` when no runtime could be resolved to probe (direct-fallback).
    pub mailbox_foreground_free: Option<bool>,
    /// #5176: the primary Discord message ids that were in the pending queue
    /// before the cancel and are gone after it. `queue_preserved=false` told an
    /// operator that something was lost; this tells them WHAT, so a violation of
    /// the user-message-lossless contract can be traced to a specific
    /// instruction instead of a decremented counter.
    pub queue_dropped_message_ids: Vec<u64>,
}

impl TurnLifecycleStopResult {
    /// The host guard kept the session: nothing was stopped, cleared or killed, and the caller
    /// must change nothing more.
    pub(crate) fn host_guard_kept(&self) -> bool {
        self.lifecycle_path == HOST_GUARD_KEPT_PATH
    }

    pub(crate) fn queue_depth_if_observed(&self) -> Option<usize> {
        self.queue_depth
            .filter(|_| self.queue_depth_after.is_some())
    }
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
        None,
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
        None,
        reason,
        termination_reason_code,
        true,
    )
    .await
}

/// The sessions row a force-kill caller read its target from, with its raw provider.
#[derive(Clone, Copy)]
pub(crate) struct ForceKillRow<'a> {
    pub pool: &'a sqlx::PgPool,
    pub session_key: &'a str,
    pub stored_provider: Option<&'a str>,
}

/// [`force_kill_turn`] for a caller holding its target's row: with no runtime to key the
/// session, that row alone decides whether the host guard admits the kill.
pub(crate) async fn force_kill_turn_for_row(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    row: ForceKillRow<'_>,
    reason: &str,
    termination_reason_code: &'static str,
) -> TurnLifecycleStopResult {
    let code = termination_reason_code;
    force_kill_turn_with_cancel_event(health_registry, target, Some(row), reason, code, true).await
}

pub(crate) async fn force_kill_turn_without_cancel_event(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    row: Option<ForceKillRow<'_>>,
    reason: &str,
    termination_reason_code: &'static str,
) -> TurnLifecycleStopResult {
    force_kill_turn_with_cancel_event(
        health_registry,
        target,
        row,
        reason,
        termination_reason_code,
        false,
    )
    .await
}

async fn force_kill_turn_with_cancel_event(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    row: Option<ForceKillRow<'_>>,
    reason: &str,
    termination_reason_code: &'static str,
    emit_cancel_observability: bool,
) -> TurnLifecycleStopResult {
    let verdict = force_kill_verdict(health_registry, target, row).await;
    let code = termination_reason_code;
    let emit = emit_cancel_observability;
    force_kill_on_verdict(health_registry, verdict, reason, code, emit).await
}

/// A force-kill's host verdict with the target and tmux name it judged, taken before the
/// caller changes anything; the kill it admits runs on these values without judging again.
#[must_use]
pub(crate) struct ForceKillVerdict {
    target: TurnLifecycleTarget,
    observed: Option<String>,
    backfill: bool,
    host: ForceKillHost,
}

impl ForceKillVerdict {
    /// The host guard keeps the session: the caller must change nothing for this kill.
    pub(crate) fn kept(&self) -> bool {
        matches!(self.host, ForceKillHost::Gate(ChannelTeardown::Kept, _))
    }
}

/// The host verdict a force-kill of `target` gets, read without writing anything.
pub(crate) async fn force_kill_verdict(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    row: Option<ForceKillRow<'_>>,
) -> ForceKillVerdict {
    let (observed, backfill) = match guard_observed(health_registry, target).await {
        Ok(observed) => observed,
        Err(error) => {
            tracing::warn!(
                error,
                "host guard kept a force-kill whose inflight is unreadable"
            );
            let host = ForceKillHost::Gate(ChannelTeardown::Kept, None);
            let (target, observed, backfill) = (target.clone(), None, false);
            return ForceKillVerdict {
                target,
                observed,
                backfill,
                host,
            };
        }
    };
    let name = observed.clone().filter(|name| !name.is_empty());
    let name = name.unwrap_or_else(|| target.tmux_name.clone());
    let host = force_kill_host_gate(health_registry, target, row, &name).await;
    let target = target.clone();
    ForceKillVerdict {
        target,
        observed,
        backfill,
        host,
    }
}

/// [`force_kill_verdict`] for a sessions row read as stored: its raw provider and thread.
pub(crate) async fn force_kill_row_verdict(
    registry: Option<&HealthRegistry>,
    pool: &sqlx::PgPool,
    stored_provider: Option<&str>,
    channel_id: Option<&str>,
    session_key: &str,
    tmux_name: &str,
) -> ForceKillVerdict {
    let channel_id = channel_id.and_then(|raw| raw.parse::<u64>().ok());
    let target = TurnLifecycleTarget {
        provider: stored_provider.and_then(ProviderKind::from_str),
        channel_id: channel_id.map(ChannelId::new),
        tmux_name: tmux_name.to_string(),
    };
    let row = ForceKillRow {
        pool,
        session_key,
        stored_provider,
    };
    force_kill_verdict(registry, &target, Some(row)).await
}

/// [`force_kill_turn`] of the target `verdict` judged, run on that verdict.
pub(crate) async fn force_kill_turn_with_verdict(
    health_registry: Option<&HealthRegistry>,
    verdict: ForceKillVerdict,
    reason: &str,
    termination_reason_code: &'static str,
) -> TurnLifecycleStopResult {
    let code = termination_reason_code;
    force_kill_on_verdict(health_registry, verdict, reason, code, true).await
}

async fn force_kill_on_verdict(
    health_registry: Option<&HealthRegistry>,
    verdict: ForceKillVerdict,
    reason: &str,
    termination_reason_code: &'static str,
    emit_cancel_observability: bool,
) -> TurnLifecycleStopResult {
    let target = verdict.target.clone();
    let policy = crate::services::discord::TmuxCleanupPolicy::CleanupSession {
        termination_reason_code: Some(termination_reason_code),
    };
    let emit = emit_cancel_observability;
    stop_turn_with_policy(
        health_registry,
        &target,
        Some(verdict),
        reason,
        policy,
        emit,
    )
    .await
}

async fn stop_turn_with_policy(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    verdict: Option<ForceKillVerdict>,
    reason: &str,
    cleanup_policy: crate::services::discord::TmuxCleanupPolicy,
    emit_cancel_observability: bool,
) -> TurnLifecycleStopResult {
    // #1672: capture the *observed* tmux session name and the disk/memory
    // pending-queue snapshot before we touch anything. The cancel-API
    // response and the cancel observability event both want
    // post-fact-accurate fields, not the hardcoded "queue_preserved=true"
    // contract that masked the 2026-05-04 ch-dd queue-loss incident.
    // A force-kill runs on the verdict taken before it, so nothing here judges the host again.
    // A preserve stop judges the channel's turn before its first write and keeps a refused host.
    let keys = (health_registry, target.provider.as_ref(), target.channel_id);
    let judged = match (
        verdict.is_none() && !cleanup_policy.should_cleanup_tmux(),
        keys,
    ) {
        (true, (Some(registry), Some(provider), Some(channel))) => {
            let judge = crate::services::discord::health::judge_provider_channel_stop;
            Some(judge(registry, provider.as_str(), channel).await)
        }
        _ => None,
    };
    if let Some(judged) = judged.as_ref().filter(|judged| judged.host_refused()) {
        return kept_by_host_guard(judged.session());
    }
    let (tmux_session_observed, backfill, host) = match verdict {
        Some(verdict) => (verdict.observed, verdict.backfill, verdict.host),
        None if cleanup_policy.should_cleanup_tmux() => return kept_by_host_guard(None),
        None => {
            let observed = resolve_tmux_session_observed(health_registry, target).await;
            (observed, false, ForceKillHost::NotKill)
        }
    };
    let probe_session_owned = tmux_session_observed
        .clone()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| target.tmux_name.clone());
    if matches!(host, ForceKillHost::Gate(ChannelTeardown::Kept, _)) {
        return kept_by_host_guard(tmux_session_observed);
    }
    // A kill on a channel's runtime stops only the runtime and session its verdict approved.
    let approved = host.approved(&probe_session_owned);
    if let (true, Some(_), Some(provider), Some(channel)) =
        (cleanup_policy.should_cleanup_tmux(), keys.0, keys.1, keys.2)
    {
        let holds = match approved {
            Some((shared, name)) => runtime_target_holds(shared, provider, channel, name).await,
            None => false,
        };
        if !holds {
            tracing::warn!(
                ?channel,
                "host guard kept a force-kill: its runtime moved on"
            );
            return kept_by_host_guard(tmux_session_observed);
        }
    }
    if let (true, Some(provider), Some(channel)) = (backfill, &target.provider, target.channel_id) {
        backfill_inflight_after_guard(provider, channel);
    }
    if let Some(channel_id) = target.channel_id {
        let tmux_session_name = (!target.tmux_name.is_empty()).then_some(target.tmux_name.as_str());
        crate::services::discord::record_turn_stop_tombstone(channel_id, tmux_session_name, reason)
            .await;
    }
    let pre_snapshot = pending_queue_pre_snapshot(health_registry, target).await;

    let mut lifecycle_path = DIRECT_FALLBACK_PATH;
    let mut queue_depth = None;
    let mut termination_recorded = false;
    let mut runtime_persistent_inflight_cleared = false;
    let mut mailbox_foreground_free = None;
    let tmux_was_alive = !probe_session_owned.is_empty()
        && crate::services::platform::tmux::has_session(&probe_session_owned);
    let cleanup_tmux = cleanup_policy.should_cleanup_tmux();

    if let (Some(_), Some(provider), Some(channel_id)) =
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
            let policy = crate::services::discord::TmuxCleanupPolicy::CleanupSession {
                termination_reason_code: Some(termination_reason_code),
            };
            let stop = crate::services::discord::health::stop_channel_runtime;
            match approved {
                Some((shared, name)) => {
                    Some(stop(shared, provider, channel_id, reason, policy, Some(name)).await)
                }
                None => None,
            }
        } else if let Some(judged) = judged {
            let stop = crate::services::discord::health::stop_judged_provider_channel;
            stop(judged, reason, cleanup_policy).await
        } else {
            None
        };
        if let Some(runtime) = runtime {
            use crate::services::discord::health::InflightDisposition;
            // A stop that was not a legacy one leaves the turn's row and session to its owner.
            if runtime.inflight == InflightDisposition::PreservedByHostGuard {
                return kept_by_host_guard(tmux_session_observed);
            }
            lifecycle_path = runtime.lifecycle_path;
            queue_depth = Some(runtime.queue_depth);
            termination_recorded = runtime.termination_recorded;
            runtime_persistent_inflight_cleared = runtime.inflight == InflightDisposition::Cleared;
            mailbox_foreground_free = Some(runtime.mailbox_foreground_free);
        }
    }

    // A preserve stop that only knows the tmux name clears the runtime turn found by that name.
    // A force-kill never looks a runtime up by name: with none approved it acts on its row only.
    if lifecycle_path == DIRECT_FALLBACK_PATH
        && !cleanup_tmux
        && let Some(registry) = health_registry
    {
        let hard_stop = crate::services::discord::health::stop_runtime_turn_preserving_watcher(
            Some(registry),
            target.provider.as_ref().map(|provider| provider.as_str()),
            target.channel_id.map(|channel_id| channel_id.get()),
            Some(&target.tmux_name),
            "turn_lifecycle_preserve_direct_fallback",
        )
        .await;
        if hard_stop.cleanup_path != "runtime_unavailable_fallback" {
            lifecycle_path = hard_stop.cleanup_path;
        }
    }
    // A turn whose finish went unobserved is kept; a force-kill never kills a superseded turn's host.
    match lifecycle_path {
        HOST_GUARD_KEPT_PATH | FINISH_UNOBSERVED_PATH => {
            return kept_by_host_guard(tmux_session_observed);
        }
        TOKEN_SUPERSEDED_PATH if cleanup_tmux => return kept_by_host_guard(tmux_session_observed),
        _ => {}
    }

    let tmux_killed = if cleanup_tmux {
        let kill_target = if !probe_session_owned.is_empty() {
            probe_session_owned.as_str()
        } else {
            target.tmux_name.as_str()
        };
        #[cfg(unix)]
        if crate::services::platform::tmux::has_session(kill_target) {
            record_tmux_exit_reason(kill_target, &format!("explicit cleanup via {reason}"));
        }

        #[cfg(test)]
        let preserve_for_test = force_kill_preserves_tmux_for_test(kill_target);
        #[cfg(not(test))]
        let preserve_for_test = false;
        let killed_now = if preserve_for_test {
            false
        } else if crate::services::platform::tmux::has_session(kill_target) {
            crate::services::platform::tmux::kill_session(
                kill_target,
                &format!("explicit cleanup via {reason}"),
            )
        } else {
            tmux_was_alive
        };
        // Delete persistent + legacy session temp files alongside the kill
        // so /tmp and ~/.adk/release/runtime/sessions/ don't accumulate
        // stale jsonl/FIFO/owner markers after forced termination (#892).
        if killed_now {
            match &host {
                ForceKillHost::Gate(ChannelTeardown::Cleared(session), _) => {
                    crate::services::tmux_common::cleanup_cleared_session_temp_files(session)
                }
                _ => crate::services::tmux_common::cleanup_session_temp_files(kill_target),
            }
        }
        killed_now
    } else {
        // #1672: even with a "preserve session" policy, the underlying
        // C-c → SIGKILL → child cleanup path can take the tmux session
        // down (e.g. the wrapper for Claude TUI also dies when claude
        // exits). Re-check after the stop so the cancel API response
        // stops misreporting `tmux_killed=false` for sessions that died.
        tmux_was_alive
            && !probe_session_owned.is_empty()
            && !crate::services::platform::tmux::has_session(&probe_session_owned)
    };

    // Only the target's own channel row: a row found by tmux name may be another runtime's.
    let inflight_cleared = if runtime_persistent_inflight_cleared {
        true
    } else if lifecycle_path != TOKEN_SUPERSEDED_PATH && cleanup_policy.should_clear_inflight() {
        let keys = (target.provider.as_ref(), target.channel_id);
        let clear = |(provider, channel_id)| clear_inflight_by_channel(provider, channel_id);
        keys.0.zip(keys.1).is_some_and(clear)
    } else {
        false
    };

    // #1672: assert the queue-preservation invariant by observation, not
    // by hardcoded contract. A canonical/runtime cancel that quietly
    // drained the pending_queue (the very bug this issue is about) now
    // produces `queue_preserved=false` so operators can spot it from the
    // API response or the cancel observability event.
    let post_snapshot = pending_queue_post_snapshot(health_registry, target).await;
    let queue_preserved = compute_queue_preserved(
        cleanup_policy,
        pre_snapshot.as_ref(),
        post_snapshot.as_ref(),
    );
    let queue_dropped_message_ids =
        dropped_queue_message_ids(pre_snapshot.as_ref(), post_snapshot.as_ref());

    if health_registry.is_some() && (pre_snapshot.is_none() || post_snapshot.is_none()) {
        tracing::warn!(channel = ?target.channel_id, "pending queue unobservable across cancel");
    }
    let result = TurnLifecycleStopResult {
        lifecycle_path,
        tmux_killed,
        inflight_cleared,
        queue_depth,
        queue_preserved,
        termination_recorded,
        tmux_session_observed,
        queue_depth_before: pre_snapshot.as_ref().map(|s| s.queue_depth),
        queue_depth_after: post_snapshot.as_ref().map(|s| s.queue_depth),
        queue_disk_present_before: pre_snapshot.as_ref().and_then(|s| s.disk_present),
        queue_disk_present_after: post_snapshot.as_ref().and_then(|s| s.disk_present),
        mailbox_foreground_free,
        queue_dropped_message_ids,
    };

    // #5176: a cancel that silently ate a queued user instruction violates the
    // user-message-lossless contract. Name the casualties at ERROR — a
    // decremented `queued_remaining` in an INFO line is not a report.
    if !result.queue_dropped_message_ids.is_empty() {
        tracing::error!(
            provider = target
                .provider
                .as_ref()
                .map(ProviderKind::as_str)
                .unwrap_or("unknown"),
            channel_id = target.channel_id.map(ChannelId::get).unwrap_or(0),
            lifecycle_path,
            reason,
            dropped_message_ids = ?result.queue_dropped_message_ids,
            queue_depth_before = ?result.queue_depth_before,
            queue_depth_after = ?result.queue_depth_after,
            "cancel dropped queued user messages instead of preserving them (see #5176)"
        );
    }

    // #5176: a cancel that stamped the turn `cancelled` while the mailbox kept
    // its foreground anchor is not a successful cancel — it is the incident.
    // Say so at ERROR so it can never again be read as success from the logs.
    if mailbox_foreground_free == Some(false) {
        tracing::error!(
            provider = target
                .provider
                .as_ref()
                .map(ProviderKind::as_str)
                .unwrap_or("unknown"),
            channel_id = target.channel_id.map(ChannelId::get).unwrap_or(0),
            lifecycle_path,
            reason,
            "cancel stamped the turn cancelled but the mailbox still owns the foreground slot; \
             the channel remains blocked (see #5176)"
        );
    }

    if emit_cancel_observability {
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
pub(crate) mod policy_observability_tests {
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

    // SAFETY (await_holding_lock): `observability::test_runtime_lock()` is a std
    // Mutex held across awaits to serialize tests that reset/init the
    // process-global observability runtime; the hold must span the awaits to
    // keep concurrent tests from racing on the shared runtime. Test-only.
    #[tokio::test]
    async fn cancel_observability_emits_unknown_noop_direct_fallback() {
        let _ = crate::services::observability::events::test_capture::capture_async(
            cancel_observability_emits_unknown_noop_direct_fallback_scenario(|| {}),
        )
        .await;
    }

    #[allow(clippy::await_holding_lock)]
    pub(crate) async fn cancel_observability_emits_unknown_noop_direct_fallback_scenario(
        before_observe: impl FnOnce(),
    ) {
        let _guard = crate::services::observability::test_runtime_lock();
        crate::services::observability::reset_for_tests();
        crate::services::observability::init_observability(None);

        let target = super::TurnLifecycleTarget {
            provider: Some(crate::services::provider::ProviderKind::Codex),
            channel_id: None,
            tmux_name: format!(
                "AgentDesk-missing-noop-cancel-observability-{}",
                std::process::id()
            ),
        };
        let result =
            super::stop_turn_preserving_queue(None, &target, "queue-api cancel_turn (preserve)")
                .await;
        assert_eq!(result.lifecycle_path, super::DIRECT_FALLBACK_PATH);
        assert!(!result.tmux_killed);
        assert!(!result.inflight_cleared);
        assert_eq!(result.queue_depth, None);
        assert!(!result.termination_recorded);

        before_observe();
        assert_noop_cancel_event();
    }

    pub(crate) fn assert_noop_cancel_event() {
        let event = crate::services::observability::events::test_capture::one("turn_cancelled");
        assert_eq!(event.channel_id, None);
        assert_eq!(event.provider.as_deref(), Some("codex"));
        assert_eq!(event.payload["reason"], "queue-api cancel_turn (preserve)");
        assert_eq!(event.payload["surface"], "queue_cancel_preserve");
        assert_eq!(event.payload["lifecyclePath"], super::DIRECT_FALLBACK_PATH);
        assert_eq!(event.payload["emittedNoOp"], true);
        assert!(event.payload["dispatch_id"].is_null());
        assert!(event.payload["session_key"].is_null());
        assert!(event.payload["turn_id"].is_null());
    }

    #[tokio::test]
    async fn queue_truth_lost_actor_is_not_a_measured_empty_queue() {
        use super::*;
        use crate::services::turn_orchestrator::{ChannelMailboxRegistry, QueuePersistenceContext};
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let registry = HealthRegistry::new();
        registry.register("claude".into(), shared.clone()).await;
        let (mailboxes, token) = shared.queue_fixture_parts();
        for (i, drop_reply) in [false, true].into_iter().enumerate() {
            let channel = ChannelId::new(6038740 + i as u64);
            let target = super::TurnLifecycleTarget {
                provider: Some(ProviderKind::Claude),
                channel_id: Some(channel),
                tmux_name: String::new(),
            };
            let handle = mailboxes.handle(channel);
            let item = ChannelMailboxRegistry::queued_for_test(42);
            handle
                .replace_queue(
                    vec![item],
                    QueuePersistenceContext::new(&ProviderKind::Claude, token, None),
                )
                .await;
            let pre = super::pending_queue_pre_snapshot(Some(&registry), &target)
                .await
                .unwrap();
            assert_eq!(pre.queue_depth, 1);
            if drop_reply {
                mailboxes.insert_reply_dropping_for_test(channel);
            } else {
                mailboxes.insert_unreachable_for_test(channel);
            }
            let post = super::pending_queue_post_snapshot(Some(&registry), &target).await;
            mailboxes.remove_fixture_for_test(channel);
            assert!(post.is_none());
            assert_eq!(
                super::compute_queue_preserved(
                    TmuxCleanupPolicy::PreserveSession,
                    Some(&pre),
                    post.as_ref()
                ),
                None
            );
            assert!(super::dropped_queue_message_ids(Some(&pre), post.as_ref()).is_empty());
        }
    }

    #[tokio::test]
    async fn queue_truth_disk_stat_error_is_unmeasured() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let registry = HealthRegistry::new();
        registry.register("claude".into(), shared.clone()).await;
        let (mailboxes, token) = shared.queue_fixture_parts();
        let channel = ChannelId::new(6038742);
        let parent = crate::services::discord::runtime_store::discord_pending_queue_root()
            .unwrap()
            .join("claude");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(parent.join(token), b"not a directory").unwrap();
        let observed = crate::services::discord::health::snapshot_pending_queue_state(
            &registry, "claude", channel,
        )
        .await
        .unwrap();
        mailboxes.remove_fixture_for_test(channel);
        assert_eq!(observed.disk_present, None);
    }

    #[test]
    fn compute_queue_preserved_detects_disk_and_memory_loss() {
        use crate::services::discord::health::PendingQueueSnapshot;

        let pre = PendingQueueSnapshot {
            queue_depth: 1,
            disk_present: Some(true),
            disk_path: None,
            message_ids: vec![9_001],
        };
        let post_loss = PendingQueueSnapshot {
            queue_depth: 0,
            disk_present: Some(false),
            disk_path: None,
            message_ids: Vec::new(),
        };
        let lost_memory = PendingQueueSnapshot {
            queue_depth: 0,
            ..pre.clone()
        };
        let policy = TmuxCleanupPolicy::PreserveSession;
        assert_eq!(
            super::compute_queue_preserved(policy, Some(&pre), Some(&post_loss)),
            Some(false)
        );
        assert_eq!(
            super::compute_queue_preserved(policy, Some(&pre), Some(&pre)),
            Some(true)
        );
        assert_eq!(
            super::compute_queue_preserved(policy, Some(&pre), Some(&lost_memory)),
            Some(false)
        );
        let lost_disk = PendingQueueSnapshot {
            queue_depth: 1,
            ..post_loss.clone()
        };
        assert_eq!(
            super::compute_queue_preserved(policy, Some(&pre), Some(&lost_disk)),
            Some(false)
        );
        let empty = PendingQueueSnapshot {
            disk_present: Some(false),
            ..Default::default()
        };
        assert_eq!(
            super::compute_queue_preserved(policy, Some(&empty), Some(&empty)),
            Some(true)
        );
        for (pre, post) in [(None, None), (Some(&pre), None), (None, Some(&pre))] {
            assert_eq!(super::compute_queue_preserved(policy, pre, post), None);
        }
    }

    /// #5176: the cancel response must name the user instructions it destroyed.
    /// Depth is not enough — a swapped queue keeps the same depth.
    #[test]
    fn dropped_queue_message_ids_names_the_lost_user_instructions() {
        use crate::services::discord::health::PendingQueueSnapshot;

        fn snapshot(message_ids: Vec<u64>) -> PendingQueueSnapshot {
            PendingQueueSnapshot {
                queue_depth: message_ids.len(),
                disk_present: Some(!message_ids.is_empty()),
                disk_path: None,
                message_ids,
            }
        }

        // The #5176 incident shape: queued_before=1, queued_remaining=0.
        assert_eq!(
            super::dropped_queue_message_ids(
                Some(&snapshot(vec![9_001])),
                Some(&snapshot(Vec::new())),
            ),
            vec![9_001]
        );

        // Same depth, different message: still a lost user instruction, and the
        // reason a depth comparison alone cannot audit the lossless contract.
        assert_eq!(
            super::dropped_queue_message_ids(
                Some(&snapshot(vec![9_001])),
                Some(&snapshot(vec![9_002])),
            ),
            vec![9_001]
        );

        // Preservation, and growth, report nothing.
        assert!(
            super::dropped_queue_message_ids(
                Some(&snapshot(vec![9_001])),
                Some(&snapshot(vec![9_001, 9_002])),
            )
            .is_empty()
        );

        // An unobservable side is not evidence of a drop.
        assert!(super::dropped_queue_message_ids(Some(&snapshot(vec![9_001])), None).is_empty());
        assert!(super::dropped_queue_message_ids(None, None).is_empty());
    }
}

/// What the host guard lets a stop do before it touches anything.
#[must_use]
enum ForceKillHost {
    /// The policy keeps tmux: not a kill.
    NotKill,
    /// A keyed channel holding no tmux name, on the runtime the nameless gate admits.
    Nameless(Arc<SharedData>),
    /// The gate's verdict; a kill it admits on a channel's runtime holds that runtime.
    Gate(ChannelTeardown, Option<Arc<SharedData>>),
}

impl ForceKillHost {
    /// The runtime and session name an admitted kill on a channel's runtime may stop.
    fn approved<'a>(&'a self, name: &'a str) -> Option<(&'a Arc<SharedData>, Option<&'a str>)> {
        match self {
            Self::Nameless(shared) => Some((shared, None)),
            Self::Gate(_, Some(shared)) => Some((shared, Some(name))),
            Self::NotKill | Self::Gate(_, None) => None,
        }
    }
}

/// The host guard for a force-kill: on a channel's runtime, that runtime's targets with the
/// caller's row, the runtime's key and the channel's row; else the caller's own row alone.
async fn force_kill_host_gate(
    registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
    row: Option<ForceKillRow<'_>>,
    tmux_name: &str,
) -> ForceKillHost {
    let caller = "turn_lifecycle_force_kill";
    match (registry, target.provider.as_ref(), target.channel_id) {
        (Some(registry), Some(provider), Some(channel)) if tmux_name.is_empty() => {
            match nameless_runtime_teardown(registry, provider, channel, caller).await {
                Some(shared) => ForceKillHost::Nameless(shared),
                None => ForceKillHost::Gate(ChannelTeardown::Kept, None),
            }
        }
        (Some(registry), Some(provider), Some(channel)) => {
            let key = row.map(|row| row.session_key);
            let gate = runtime_teardown(registry, provider, channel, tmux_name, key, caller);
            let (gate, runtime) = gate.await;
            ForceKillHost::Gate(gate, runtime)
        }
        _ => match row.filter(|_| !tmux_name.is_empty()) {
            Some(row) => {
                let (provider, key) = (row.stored_provider, row.session_key);
                let channel = target.channel_id.map_or(0, ChannelId::get);
                let gate = row_gate(row.pool, provider, channel, key, tmux_name, caller);
                ForceKillHost::Gate(gate.await.0, None)
            }
            None => {
                tracing::warn!(
                    caller,
                    tmux_name,
                    "host guard kept a force-kill holding no key"
                );
                ForceKillHost::Gate(ChannelTeardown::Kept, None)
            }
        },
    }
}

/// A stop the host guard kept: nothing was stopped, cleared or killed, and the caller must
/// change nothing more.
fn kept_by_host_guard(tmux_session_observed: Option<String>) -> TurnLifecycleStopResult {
    TurnLifecycleStopResult {
        lifecycle_path: HOST_GUARD_KEPT_PATH,
        tmux_killed: false,
        inflight_cleared: false,
        queue_depth: None,
        queue_preserved: None,
        termination_recorded: false,
        tmux_session_observed,
        queue_depth_before: None,
        queue_depth_after: None,
        queue_disk_present_before: None,
        queue_disk_present_after: None,
        mailbox_foreground_free: None,
        queue_dropped_message_ids: Vec::new(),
    }
}

fn clear_inflight_by_channel(provider: &ProviderKind, channel_id: ChannelId) -> bool {
    crate::services::discord::clear_inflight_state(provider, channel_id.get())
}

/// #1672: best-effort tmux session name lookup at cancel time. Used by
/// the cancel API response so `tmux_session` can never be reported as
/// `""` while the runtime knows perfectly well which session is being
/// stopped.
/// [`resolve_tmux_session_observed`] for a force-kill, read only; the flag asks for the
/// inflight backfill the normal lookup writes once the guard admits.
async fn guard_observed(
    registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
) -> Result<(Option<String>, bool), String> {
    if !target.tmux_name.is_empty() {
        return Ok((Some(target.tmux_name.clone()), false));
    }
    let keys = (registry, target.provider.as_ref(), target.channel_id);
    let (Some(registry), Some(provider), Some(channel)) = keys else {
        return Ok((None, false));
    };
    guard_tmux_name(registry, provider, channel).await
}

async fn resolve_tmux_session_observed(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
) -> Option<String> {
    if !target.tmux_name.is_empty() {
        return Some(target.tmux_name.clone());
    }
    let registry = health_registry?;
    let provider = target.provider.as_ref()?;
    let channel_id = target.channel_id?;
    crate::services::discord::health::resolve_tmux_session_for_cancel(
        registry,
        provider.as_str(),
        channel_id,
    )
    .await
}

async fn pending_queue_pre_snapshot(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
) -> Option<crate::services::discord::health::PendingQueueSnapshot> {
    let registry = health_registry?;
    let provider = target.provider.as_ref()?;
    let channel_id = target.channel_id?;
    crate::services::discord::health::snapshot_pending_queue_state(
        registry,
        provider.as_str(),
        channel_id,
    )
    .await
}

async fn pending_queue_post_snapshot(
    health_registry: Option<&HealthRegistry>,
    target: &TurnLifecycleTarget,
) -> Option<crate::services::discord::health::PendingQueueSnapshot> {
    pending_queue_pre_snapshot(health_registry, target).await
}

/// #5176: the queued primary message ids that were present before the cancel
/// and absent after it.
///
/// Deliberately identity-based rather than depth-based. `queue_preserved` is an
/// OBSERVATION (that is the whole point of the #1672 fix — it used to be
/// hardcoded `true`), so "flipping the default back to preserve" would only
/// re-hide the loss it was built to expose. The actionable half of a lossless
/// contract is naming what went missing, which a depth comparison cannot do: a
/// queue that lost item A and gained item B has the same depth and is still a
/// lost user instruction.
///
/// Returns empty when either side is unobservable — an unknown queue is not
/// evidence of a drop.
fn dropped_queue_message_ids(
    pre: Option<&crate::services::discord::health::PendingQueueSnapshot>,
    post: Option<&crate::services::discord::health::PendingQueueSnapshot>,
) -> Vec<u64> {
    let (Some(pre), Some(post)) = (pre, post) else {
        return Vec::new();
    };
    pre.message_ids
        .iter()
        .copied()
        .filter(|message_id| !post.message_ids.contains(message_id))
        .collect()
}

/// Preservation requires measured memory and disk state on both sides of the cancel.
fn compute_queue_preserved(
    cleanup_policy: crate::services::discord::TmuxCleanupPolicy,
    pre: Option<&crate::services::discord::health::PendingQueueSnapshot>,
    post: Option<&crate::services::discord::health::PendingQueueSnapshot>,
) -> Option<bool> {
    let _ = cleanup_policy;
    let (pre, post) = (pre?, post?);
    let (before, after) = (pre.disk_present?, post.disk_present?);
    Some((!before || after) && post.queue_depth >= pre.queue_depth)
}

#[cfg(all(test, unix))]
mod host_guard_tests {
    use super::*;
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, busy_turn, channel_key, runtime, runtime_state, seed, stop_recorded, turn_kept,
    };

    // A force-kill the registry can key reads the stored rows before its tombstone, stop
    // or kill; a missing row keeps main's name-only path, any other trace keeps the turn.
    #[tokio::test]
    async fn force_kill_stops_a_turn_only_after_the_host_guard_admits_it_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (shared, registry) = runtime(&pool).await;
        for (n, stored) in Stored::ALL.into_iter().enumerate() {
            let channel = ChannelId::new(1_479_671_301_387_059_800 + n as u64);
            let name = format!("AgentDesk-claude-p4a-kill-{n}");
            seed(
                &pool,
                &channel_key(&shared, &name),
                &name,
                channel.get(),
                stored,
            )
            .await;
            let token = busy_turn(&shared, channel, &name).await;
            let target = TurnLifecycleTarget {
                provider: Some(ProviderKind::Claude),
                channel_id: Some(channel),
                tmux_name: name.clone(),
            };
            let result = force_kill_turn(Some(&registry), &target, "p4a host guard", "p4a").await;
            let admitted = matches!(stored, Stored::Legacy | Stored::Missing);
            assert_eq!(
                result.lifecycle_path != HOST_GUARD_KEPT_PATH,
                admitted,
                "{stored:?}"
            );
            assert_eq!(
                !turn_kept(&shared, channel, &token).await,
                admitted,
                "{stored:?}"
            );
            assert_eq!(stop_recorded(channel), admitted, "{stored:?}");
        }
        pool.close().await;
        db.drop().await;
    }

    // A kill runs only on the session its verdict approved: a runtime that took up another
    // session between the verdict and the kill is left as it is.
    #[tokio::test]
    async fn force_kill_leaves_a_runtime_that_moved_on_after_its_verdict_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (shared, registry) = runtime(&pool).await;
        let channel = ChannelId::new(1_479_671_301_387_059_900);
        let name = "AgentDesk-claude-p4r-moved-a";
        let key = channel_key(&shared, name);
        seed(&pool, &key, name, channel.get(), Stored::Legacy).await;
        let token = busy_turn(&shared, channel, name).await;
        let target = TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: name.to_string(),
        };
        let verdict = force_kill_verdict(Some(&registry), &target, None).await;
        assert!(!verdict.kept(), "the legacy session is admitted");
        let moved = "AgentDesk-claude-p4r-moved-b";
        crate::services::discord::register_resume_watcher_for_tests(&shared, channel, moved);
        let before = runtime_state(&shared, channel).await;
        let kill = force_kill_turn_with_verdict(Some(&registry), verdict, "moved on", "p4r");
        assert_eq!(kill.await.lifecycle_path, HOST_GUARD_KEPT_PATH);
        assert_eq!(runtime_state(&shared, channel).await, before);
        assert!(turn_kept(&shared, channel, &token).await);
        assert!(!stop_recorded(channel), "no stop is recorded");
        pool.close().await;
        db.drop().await;
    }

    // A kill carries out its verdict: a marker that turns Herdr after the verdict does not make
    // the cleanup judge the host again, while a cleanup with no verdict still reads it.
    #[test]
    fn force_kill_follows_its_verdict_when_the_marker_changes_after_it_pg() {
        use crate::services::provider::cancel_token_cleanup::executor::{
            CleanupRequest, TmuxCleanupIntent, tmux_kill_dispatches_for_test,
            with_executor_dispatch_seam,
        };
        // The runtime root (env lock) comes before the dispatch seam, as in the stop host tests.
        let _root = crate::config::TestRuntimeRootGuard::new();
        let mut executor = tokio::runtime::Builder::new_current_thread();
        let executor = executor.enable_all().build().unwrap();
        with_executor_dispatch_seam(|| {
            executor.block_on(async {
                let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
                let pool = db.connect_and_migrate().await;
                let (shared, registry) = runtime(&pool).await;
                let channel = ChannelId::new(1_479_671_301_387_059_950);
                let name = "AgentDesk-claude-p4r-marker-after";
                let key = channel_key(&shared, name);
                seed(&pool, &key, name, channel.get(), Stored::Legacy).await;
                busy_turn(&shared, channel, name).await;
                let target = TurnLifecycleTarget {
                    provider: Some(ProviderKind::Claude),
                    channel_id: Some(channel),
                    tmux_name: name.to_string(),
                };
                let verdict = force_kill_verdict(Some(&registry), &target, None).await;
                assert!(!verdict.kept(), "the legacy session is admitted");
                let marker = crate::services::tmux_common::session_temp_path(name, "host_kind");
                std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
                std::fs::write(&marker, "herdr").unwrap();
                let kills = tmux_kill_dispatches_for_test();
                let kill = force_kill_turn_with_verdict(Some(&registry), verdict, "marker", "p4r");
                assert!(!kill.await.host_guard_kept());
                let dispatched = tmux_kill_dispatches_for_test() - kills;
                assert_eq!(
                    dispatched, 1,
                    "the approved session is killed once, by its cleanup"
                );
                let other = crate::services::provider::CancelToken::new();
                other.bind_unmanaged_session_name(name);
                let outcome = other.request_cleanup(CleanupRequest {
                    cancel_source: "no verdict".to_string(),
                    intent: TmuxCleanupIntent::CleanupSession,
                    termination_reason: Some("p4r"),
                    hard_stop_target: None,
                });
                assert!(
                    outcome.host_refused,
                    "a cleanup with no verdict still reads the marker"
                );
                assert_eq!(tmux_kill_dispatches_for_test() - kills, 1);
                pool.close().await;
                db.drop().await;
            })
        });
    }
}
