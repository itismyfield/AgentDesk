//! Relay recovery dry-run planner and conservative auto-heal executor.
//!
//! This module is intentionally narrow: it turns the read-only relay health
//! classifier into an operator-facing decision, and only applies local,
//! idempotent cleanup when the evidence is strong enough.
//!
//! Known residual limitations for follow-up issues: committed-but-leaked and
//! stale foreign inflight rows are swept independently of TUI-direct pending-start
//! records, while retaining the same terminal/death-evidence and identity gates.
//! Rows whose `output_path` is missing
//! or points at a deleted file are permanently denied by the destructive cancel
//! gate because no frozen-capture or terminal-envelope evidence can be re-probed.
//! Stage-3 recovery where `watcher_attached=false` still relies on the
//! pending-start backstop trigger. Frozen-busy JSONL rows remain denied until
//! the output file has been quiescent for the conservative stale window and the
//! live pane itself reports ready for input; shorter freezes or busy panes are
//! intentionally residual. Committed rows coupled to a mismatched `rebind_origin`
//! are not independently healed here. The manual stale-mailbox repair route
//! additionally requires `unread_bytes == 0` (parity with ReattachWatcher): a
//! dead relay that leaves capture bytes permanently ahead of the relay offset
//! keeps that manual path blocked even when the pane is ready — resolving such
//! rows falls to the destructive cancel gate / pending-start demote instead.
//! Do not broaden those paths inside the
//! #4030 watcher-cancel fix; they need separate design/review.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use poise::serenity_prelude::ChannelId;
use serde::Serialize;

use super::health::HealthRegistry;
use super::relay_health::{RelayActiveTurn, RelayHealthSnapshot, RelayStallState};
use super::{
    SharedData, clear_watchdog_deadline_override, destructive_cancel_gate, health, inflight,
    mailbox_clear_channel, mailbox_clear_recovery_marker, mailbox_finish_turn, mailbox_snapshot,
    recovery, saturating_decrement_global_active, stall_recovery, turn_finalizer,
};
use crate::services::provider::ProviderKind;

#[path = "relay_recovery/apply.rs"]
mod apply;
#[path = "relay_recovery_auto_heal_apply.rs"]
mod auto_heal_apply;
#[path = "relay_recovery_auto_heal_attempts.rs"]
mod auto_heal_attempts;
#[path = "relay_recovery_auto_heal_confirm.rs"]
mod auto_heal_confirm;
#[path = "relay_recovery_circuit_breaker.rs"]
mod circuit_breaker;
#[path = "relay_recovery_completion_footer.rs"]
mod completion_footer;
#[path = "relay_recovery/decision.rs"]
mod decision;
#[path = "relay_recovery/idle_tmux.rs"]
mod idle_tmux;
#[path = "relay_recovery_leaked_row_sweep.rs"]
pub(super) mod leaked_row_sweep;
#[path = "relay_recovery_reattach_apply.rs"]
mod reattach_apply;
#[path = "relay_recovery_circuit_alert_producer.rs"]
mod relay_recovery_circuit_alert_producer;

pub(super) use apply::*;
pub(in crate::services::discord) use decision::*;
pub(crate) use idle_tmux::*;

use auto_heal_apply::apply_relay_recovery_plan;
#[cfg(test)]
use auto_heal_attempts::{
    AUTO_HEAL_DEAD_FRONTIER_REATTACH_MAX_ATTEMPTS_PER_WINDOW,
    AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW, auto_heal_test_lock,
    clear_auto_heal_attempts_for_tests, reserve_auto_heal_attempt,
};
use auto_heal_attempts::{
    AUTO_HEAL_WINDOW_SECS, auto_heal_key, max_attempts_per_window_for_snapshot,
    remaining_auto_heal_attempts,
};

const FROZEN_BUSY_JSONL_READY_FALLBACK_AGE: Duration = Duration::from_secs(10 * 60);
/// Protect probe and manual cleanup across the #4569 incident window: mailbox
/// admission at 05:16:44.468 was misclassified at 05:16:47.320 (~2.9 seconds).
/// The 30-second margin plus the 30-second probe cadence reclaims a genuine
/// orphan on the first post-grace tick (normally within 60 seconds), not at the
/// grace boundary itself. Stall-watchdog cleanup is exempt because its caller
/// has already passed the independent death-evidence gate. A wall-clock rollback
/// extends this protection because age uses `saturating_sub` below.
const ORPHAN_PENDING_TOKEN_ADMISSION_GRACE: Duration = Duration::from_secs(30);

#[cfg(test)]
type IdleTmuxReattachInflightCandidateHook =
    Arc<dyn Fn(&super::inflight::InflightTurnState) + Send + Sync + 'static>;

#[cfg(test)]
static IDLE_TMUX_REATTACH_INFLIGHT_CANDIDATE_HOOK: OnceLock<
    Mutex<Option<IdleTmuxReattachInflightCandidateHook>>,
> = OnceLock::new();

#[cfg(test)]
fn idle_tmux_reattach_inflight_candidate_hook()
-> &'static Mutex<Option<IdleTmuxReattachInflightCandidateHook>> {
    IDLE_TMUX_REATTACH_INFLIGHT_CANDIDATE_HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
struct IdleTmuxReattachInflightCandidateHookGuard {
    previous: Option<IdleTmuxReattachInflightCandidateHook>,
}

#[cfg(test)]
impl Drop for IdleTmuxReattachInflightCandidateHookGuard {
    fn drop(&mut self) {
        let mut hook = idle_tmux_reattach_inflight_candidate_hook()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *hook = self.previous.take();
    }
}

#[cfg(test)]
fn set_idle_tmux_reattach_inflight_candidate_hook_for_tests(
    hook: IdleTmuxReattachInflightCandidateHook,
) -> IdleTmuxReattachInflightCandidateHookGuard {
    let mut slot = idle_tmux_reattach_inflight_candidate_hook()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let previous = slot.replace(hook);
    IdleTmuxReattachInflightCandidateHookGuard { previous }
}

pub(in crate::services::discord) async fn run_relay_recovery(
    registry: &HealthRegistry,
    provider_filter: Option<&str>,
    channel_id: u64,
    apply: bool,
) -> Result<RelayRecoveryResponse, RelayRecoveryError> {
    let parsed_provider = match provider_filter.map(str::trim).filter(|raw| !raw.is_empty()) {
        Some(provider) => Some(
            ProviderKind::from_str(provider)
                .ok_or_else(|| RelayRecoveryError::InvalidProvider(provider.to_string()))?,
        ),
        None => None,
    };

    let snapshot = match parsed_provider.as_ref() {
        Some(provider) => {
            registry
                .snapshot_watcher_state_for_provider(provider, channel_id)
                .await
        }
        None => registry.snapshot_watcher_state(channel_id).await,
    }
    .ok_or_else(|| RelayRecoveryError::SnapshotNotFound {
        channel_id,
        provider: provider_filter.map(str::to_string),
    })?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut decision =
        plan_relay_recovery(&snapshot.relay_health, snapshot.relay_stall_state, now_ms);
    decision.affected.finalizer_turn_id = snapshot.inflight_finalizer_turn_id;
    trace_relay_recovery_decision(&decision, apply);

    if !apply {
        return Ok(RelayRecoveryResponse {
            ok: true,
            mode: "dry_run",
            applied: false,
            skipped: false,
            decision,
            apply_result: None,
        });
    }

    let provider = ProviderKind::from_str(&decision.provider)
        .ok_or_else(|| RelayRecoveryError::InvalidProvider(decision.provider.clone()))?;
    // Channel-aware: multi-bot deployments register several runtimes per
    // provider, so a name-only lookup would auto-heal the wrong runtime's
    // relay state for this channel.
    let shared = resolve_recovery_shared(registry, &provider, &decision)
        .await
        .ok_or_else(|| RelayRecoveryError::ProviderUnavailable(decision.provider.clone()))?;
    Ok(apply_relay_recovery_plan(
        registry,
        &shared,
        &provider,
        decision,
        now_ms,
        RelayRecoveryApplySource::Manual,
    )
    .await)
}

async fn resolve_recovery_shared(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    decision: &RelayRecoveryDecision,
) -> Option<Arc<SharedData>> {
    let channel = ChannelId::new(decision.channel_id);
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        registry.shared_for_provider_on_channel(provider, channel),
    )
    .await
    {
        Ok(Some(shared)) => Some(shared),
        Ok(None) => None,
        Err(_) => {
            tracing::warn!(
                provider = provider.as_str(),
                channel_id = decision.channel_id,
                "relay recovery provider/channel runtime resolve timed out; skipping channel-scoped recovery",
            );
            None
        }
    }
}

pub(in crate::services::discord) async fn auto_apply_relay_recovery_for_shared(
    registry: &HealthRegistry,
    shared: Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    allowed_action: RelayRecoveryActionKind,
    source: RelayRecoveryApplySource,
) -> Result<RelayRecoveryResponse, RelayRecoveryError> {
    auto_apply_relay_recovery_for_shared_at(
        registry,
        shared,
        provider,
        channel_id,
        allowed_action,
        source,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
}

async fn auto_apply_relay_recovery_for_shared_at(
    registry: &HealthRegistry,
    shared: Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    allowed_action: RelayRecoveryActionKind,
    source: RelayRecoveryApplySource,
    now_ms: i64,
) -> Result<RelayRecoveryResponse, RelayRecoveryError> {
    let snapshot = registry
        .snapshot_watcher_state_for_shared(provider, shared.clone(), channel_id)
        .await
        .ok_or_else(|| RelayRecoveryError::SnapshotNotFound {
            channel_id,
            provider: Some(provider.as_str().to_string()),
        })?;

    let mut planning_health = snapshot.relay_health.clone();
    // The watchdog death-evidence exemption only applies when the caller is
    // requesting orphan-token cleanup. A StallWatchdog caller requesting
    // ReattachWatcher (relay_dead_reattach) must keep the real snapshot stall
    // state so `plan_relay_recovery` can return `ReattachWatcher`; forcing
    // `OrphanPendingToken` here would always mismatch `allowed_action` and
    // silently disable the relay-dead reattach lane (#4569 review regression).
    let planning_stall_state = if source == RelayRecoveryApplySource::StallWatchdog
        && allowed_action == RelayRecoveryActionKind::ClearOrphanPendingToken
    {
        // The watchdog caller reaches this source only after its independent
        // death-evidence gate authorizes cleanup. Plan against that committed
        // verdict without mutating the real watcher before mailbox reclaim is
        // known to have applied.
        planning_health.tmux_session = None;
        planning_health.tmux_alive = None;
        planning_health.watcher_attached = false;
        planning_health.watcher_attached_stale = false;
        planning_health.watcher_owner_channel_id = None;
        planning_health.watcher_owns_live_relay = false;
        RelayStallState::OrphanPendingToken
    } else {
        snapshot.relay_stall_state
    };
    let mut decision = plan_relay_recovery(&planning_health, planning_stall_state, now_ms);
    if source == RelayRecoveryApplySource::StallWatchdog
        && decision.relay_stall_state == RelayStallState::OrphanPendingToken
        && decision.auto_heal.skipped_reason == Some("orphan_token_within_admission_grace")
    {
        decision.auto_heal.eligible =
            eligible_orphan_pending_token_without_admission_grace(&planning_health);
        decision.auto_heal.skipped_reason = None;
    }
    decision.affected.finalizer_turn_id = snapshot.inflight_finalizer_turn_id;
    trace_relay_recovery_decision(&decision, true);

    if decision.action != allowed_action {
        decision.auto_heal.skipped_reason = Some("auto_heal_action_not_allowed");
        trace_relay_recovery_skipped(&decision, decision.auto_heal.skipped_reason);
        return Ok(RelayRecoveryResponse {
            ok: false,
            mode: "apply",
            applied: false,
            skipped: true,
            decision,
            apply_result: None,
        });
    }

    Ok(apply_relay_recovery_plan(registry, &shared, provider, decision, now_ms, source).await)
}

fn relay_recovery_status_counts_as_applied(status: &'static str) -> bool {
    matches!(
        status,
        "applied"
            | "reattached_watcher"
            | "reuse_existing_live_watcher"
            | "reattach_confirm_startup_grace"
            | "reattach_confirm_emission_in_flight"
            | "cleared_idle_tmux_stale_turn"
            | "scheduled_pending_queue_drain"
    )
}

/// #3277 verify-2: `rebind_inflight_for_channel` reports apply honestly through the claim
/// (`claim_or_reuse_watcher`, source `"recovery_restore_inflight"`), which
/// REPLACES a cancelled / heartbeat-stale / paused / output-path-changed
/// same-session incumbent (`find_watcher_by_tmux_session` folds
/// `heartbeat_stale()` into its replace predicate — see the lifecycle
/// truth-table test) but NEVER a genuinely-live fresh-heartbeat handle (no
/// duplicate-relay vector). When the claim reused such a live incumbent
/// (`watcher_spawned == false` — e.g. the heartbeat recovered between the
/// stale-handle decision and the apply, or a reused watcher owns the session
/// under another channel), say so instead of claiming "reattached_watcher".
fn reattach_apply_status(watcher_spawned: bool) -> &'static str {
    if watcher_spawned {
        "reattached_watcher"
    } else {
        "reuse_existing_live_watcher"
    }
}

fn relay_frontier_dead_reattach_owner(decision: &RelayRecoveryDecision) -> Option<ChannelId> {
    let evidence = &decision.evidence;
    // Destructive watcher cancel is reserved for the dead-frontier shape. Once
    // relay delivered any bytes (`last_relay_offset > 0`), the old recovery
    // invariant applies: keep the turn intact and let rebind restore watcher
    // coverage instead of cancelling a potentially-live CLI turn.
    if decision.relay_stall_state != RelayStallState::TmuxAliveRelayDead
        || !evidence.desynced
        || evidence.tmux_alive != Some(true)
        || !evidence.watcher_attached
        || !evidence.watcher_owns_live_relay
        || evidence.last_relay_offset != 0
    {
        return None;
    }
    Some(ChannelId::new(
        evidence
            .watcher_owner_channel_id
            .unwrap_or(decision.channel_id),
    ))
}

fn trace_relay_recovery_decision(decision: &RelayRecoveryDecision, apply_requested: bool) {
    tracing::info!(
        target: "agentdesk::discord::relay_recovery",
        provider = decision.provider.as_str(),
        channel_id = decision.channel_id,
        relay_stall_state = decision.relay_stall_state.as_str(),
        action = decision.action.as_str(),
        auto_heal_eligible = decision.auto_heal.eligible,
        apply_requested,
        reason = decision.reason,
        "relay recovery decision"
    );
}

fn trace_relay_recovery_skipped(
    decision: &RelayRecoveryDecision,
    skipped_reason: Option<&'static str>,
) {
    tracing::warn!(
        target: "agentdesk::discord::relay_recovery",
        provider = decision.provider.as_str(),
        channel_id = decision.channel_id,
        relay_stall_state = decision.relay_stall_state.as_str(),
        action = decision.action.as_str(),
        skipped_reason = skipped_reason.unwrap_or("unknown"),
        "relay recovery auto-heal skipped"
    );
}

#[cfg(test)]
#[path = "relay_recovery/tests.rs"]
mod tests;
