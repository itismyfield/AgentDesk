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
//! additionally requires `unread_bytes == Some(0)` (parity with
//! ReattachWatcher, via `unread_tail_is_proven_drained`): a dead relay that
//! leaves capture bytes permanently ahead of the relay offset — or a tail that
//! cannot be measured against this row's frontier at all — keeps that manual
//! path blocked even when the pane is ready. Resolving such rows falls to the
//! destructive cancel gate / pending-start demote instead.
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
#[path = "relay_recovery/authority_observation.rs"]
pub(crate) mod authority_observation;
#[path = "relay_recovery_auto_heal_apply.rs"]
mod auto_heal_apply;
#[path = "relay_recovery_auto_heal_attempts.rs"]
mod auto_heal_attempts;
#[path = "relay_recovery_auto_heal_confirm.rs"]
mod auto_heal_confirm;
#[path = "relay_recovery_circuit_breaker.rs"]
mod circuit_breaker;
#[path = "relay_recovery/cohort.rs"]
pub(crate) mod cohort;
#[path = "relay_recovery_completion_footer.rs"]
mod completion_footer;
#[path = "relay_recovery/decision.rs"]
mod decision;
#[cfg(unix)]
#[path = "relay_recovery/destructive_warrant.rs"]
mod destructive_warrant;
#[cfg(unix)]
pub(in crate::services::discord) use destructive_warrant::{
    destructive_warrant_bind, structural_candidate_apply,
};
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

// The site label itself is platform-neutral data: non-unix consumers (the
// stale-turn reconciler entry points) name a site even though the evidence
// machinery behind it is unix-only.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AxisBSite {
    ProbeAutoHealReattach,
    WatchdogStaleIdle,
    WatchdogExplicitBackground,
    StaleTurnIntake,
    RelayDeadReattach,
    ProbeAutoHeal,
    PolicyTickStaleSweep,
    BootReconcileSweep,
}

#[cfg(unix)]
fn axis_b_site_for_apply(
    source: RelayRecoveryApplySource,
    action: RelayRecoveryActionKind,
) -> Option<AxisBSite> {
    match (source, action) {
        (RelayRecoveryApplySource::StallWatchdog, RelayRecoveryActionKind::ReattachWatcher) => {
            Some(AxisBSite::RelayDeadReattach)
        }
        (RelayRecoveryApplySource::ProbeAutoHeal, RelayRecoveryActionKind::ReattachWatcher) => {
            Some(AxisBSite::ProbeAutoHealReattach)
        }
        (
            RelayRecoveryApplySource::ProbeAutoHeal,
            RelayRecoveryActionKind::ClearOrphanPendingToken
            | RelayRecoveryActionKind::ClearStaleThreadProof
            | RelayRecoveryActionKind::DrainPendingQueue,
        ) => Some(AxisBSite::ProbeAutoHeal),
        (
            RelayRecoveryApplySource::StallWatchdog,
            RelayRecoveryActionKind::ClearOrphanPendingToken,
        ) => Some(AxisBSite::WatchdogExplicitBackground),
        (
            RelayRecoveryApplySource::StallWatchdog,
            RelayRecoveryActionKind::ClearStaleThreadProof,
        ) => Some(AxisBSite::WatchdogStaleIdle),
        _ => None,
    }
}

/// Whether this automatic apply can reach the pinned rebind branch.
///
/// Both automatic reattach sources reserve the current episode before apply.
/// Reservation failure returns before apply, while a successful reservation
/// supplies the episode that excludes the unpinned legacy retirement arms in
/// `relay_recovery::apply`. Source labels do not select the apply branch.
#[cfg(unix)]
fn pinned_adoption_for_apply(
    source: RelayRecoveryApplySource,
    action: RelayRecoveryActionKind,
) -> bool {
    circuit_breaker::should_use_durable_circuit(action, source)
}

#[cfg(test)]
type IdleTmuxReattachInflightCandidateHook =
    Arc<dyn Fn(&super::inflight::InflightTurnState) + Send + Sync + 'static>;
#[cfg(test)]
type DestructiveCancelPostGateHook = Arc<dyn Fn() + Send + Sync + 'static>;

#[cfg(test)]
static DESTRUCTIVE_CANCEL_POST_GATE_HOOK: OnceLock<Mutex<Option<DestructiveCancelPostGateHook>>> =
    OnceLock::new();
#[cfg(test)]
static IDLE_TMUX_REATTACH_INFLIGHT_CANDIDATE_HOOK: OnceLock<
    Mutex<Option<IdleTmuxReattachInflightCandidateHook>>,
> = OnceLock::new();

#[cfg(test)]
fn destructive_cancel_post_gate_hook() -> &'static Mutex<Option<DestructiveCancelPostGateHook>> {
    DESTRUCTIVE_CANCEL_POST_GATE_HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn run_destructive_cancel_post_gate_hook_for_tests() {
    let hook = destructive_cancel_post_gate_hook()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
struct DestructiveCancelPostGateHookGuard;

#[cfg(test)]
impl Drop for DestructiveCancelPostGateHookGuard {
    fn drop(&mut self) {
        *destructive_cancel_post_gate_hook()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

#[cfg(test)]
fn set_destructive_cancel_post_gate_hook_for_tests(
    hook: DestructiveCancelPostGateHook,
) -> DestructiveCancelPostGateHookGuard {
    *destructive_cancel_post_gate_hook()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(hook);
    DestructiveCancelPostGateHookGuard
}

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

#[cfg(not(unix))]
pub(crate) async fn automatic_stale_sweep_warrants(
    registry: Option<&HealthRegistry>,
    session_key: &str,
    provider_name: &str,
    site: AxisBSite,
) -> bool {
    // Non-unix builds have no tmux reachability evidence source, so every
    // warrant operand is absent: the warrant abstains and the structural
    // candidate is preserved (absence of evidence never manufactures a veto).
    let _ = (registry, session_key, provider_name, site);
    true
}

#[cfg(unix)]
pub(crate) async fn automatic_stale_sweep_warrants(
    registry: Option<&HealthRegistry>,
    session_key: &str,
    provider_name: &str,
    _site: AxisBSite,
) -> bool {
    let structural_candidate_apply = destructive_warrant::structural_candidate_apply(true);
    let Some(registry) = registry else {
        return structural_candidate_apply;
    };
    let Some(identity) = super::session_identity::SessionIdentity::parse(session_key) else {
        return structural_candidate_apply;
    };
    let Some(provider) = ProviderKind::from_str(provider_name) else {
        return structural_candidate_apply;
    };
    let Some((identity_provider, channel)) = identity.provider_and_channel() else {
        return structural_candidate_apply;
    };
    let Ok(channel_id) = channel.parse::<u64>() else {
        return structural_candidate_apply;
    };
    if identity_provider != provider {
        return structural_candidate_apply;
    }
    let Some(snapshot) = registry
        .snapshot_watcher_state_for_provider(&provider, channel_id)
        .await
    else {
        return structural_candidate_apply;
    };
    let destructive_warrant_bind = destructive_warrant::destructive_warrant_bind(
        structural_candidate_apply,
        RelayRecoveryActionKind::ClearStaleThreadProof,
        &provider,
        Some(&snapshot),
        false,
    );
    destructive_warrant_bind.eligible
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
    #[cfg(unix)]
    if decision.action.is_destructive() {
        if let Some(_site) = axis_b_site_for_apply(source, decision.action) {
            let structural_candidate_apply =
                destructive_warrant::structural_candidate_apply(decision.auto_heal.eligible);
            let destructive_warrant_bind = destructive_warrant::destructive_warrant_bind(
                structural_candidate_apply,
                decision.action,
                provider,
                Some(&snapshot),
                pinned_adoption_for_apply(source, decision.action),
            );
            decision.auto_heal.eligible = destructive_warrant_bind.eligible;
            if let Some(reason) = destructive_warrant_bind.skipped_reason {
                decision.auto_heal.skipped_reason = Some(reason);
            }
        } else if source != RelayRecoveryApplySource::Manual {
            // This is a closed-taxonomy programming error, not a missing warrant
            // operand. Every shipped automatic source/action pair maps above;
            // an unmapped future pair must stop before mutation until it chooses
            // an explicit site. Missing snapshot or ledger evidence still
            // abstains inside `destructive_warrant_bind`.
            decision.auto_heal.eligible = false;
            decision.auto_heal.skipped_reason = Some("axis_b_warrant_site_unmapped");
        }
    }

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

/// Only statuses whose apply performed a real transition belong here. #5021:
/// `reuse_existing_live_watcher` used to be listed, but it reports that
/// `apply_rebind` left the watcher registry as it found it — nothing spawned,
/// nothing replaced — so counting it applied made every watchdog pass look like
/// a successful heal and the auto-heal budget never reached its failure backoff.
/// That status settles through the refund arm of `settle_auto_heal_confirmation`
/// instead. The watcher registry is all this status reports on: the rebind that
/// produced it still committed its episode side effects — `DiscordSession`
/// re-registration and the existing-inflight re-adoption in
/// `commit_episode_side_effects` — before the claim reused the incumbent.
fn relay_recovery_status_counts_as_applied(status: &'static str) -> bool {
    matches!(
        status,
        "applied"
            | "reattached_watcher"
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

/// #5021: the reuse no-op stopped counting as an applied heal so the auto-heal
/// budget can back off on a repeating no-transition. The relay-dead watchdog
/// asks a different question — did its reattach lane already run for this
/// channel on this tick — so give it a separate predicate instead of letting it
/// read `applied` for both. Derived from `reattach_apply_status` so the status
/// literal stays in one place.
pub(in crate::services::discord) fn relay_recovery_status_reused_live_watcher(
    status: &str,
) -> bool {
    status == reattach_apply_status(false)
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

#[cfg(all(test, unix))]
mod axis_b_tests {
    use super::*;

    pub(super) fn quiet_snapshot_for_warrant_tests(
        channel_id: u64,
    ) -> health::WatcherStateSnapshot {
        health::WatcherStateSnapshot {
            provider: "codex".to_string(),
            attached: false,
            tmux_session: None,
            watcher_owner_channel_id: None,
            last_relay_offset: 0,
            inflight_state_present: false,
            last_relay_ts_ms: 0,
            last_capture_offset: None,
            capture_coordinate: health::liveness_authority::CaptureCoordinateObservation::missing(
                None,
            ),
            unread_bytes: None,
            desynced: false,
            reconnect_count: 0,
            inflight_started_at: None,
            inflight_updated_at: None,
            inflight_user_msg_id: None,
            inflight_current_msg_id: None,
            tmux_session_alive: None,
            has_pending_queue: false,
            mailbox_active_user_msg_id: None,
            mailbox_active_turn_nonce: None,
            bound_output_path: None,
            bound_session_id: None,
            transcript_binding_stall: "none",
            inflight_terminal_delivery_committed: false,
            inflight_identity: None,
            inflight_finalizer_turn_id: None,
            inflight_output_path: None,
            #[cfg(unix)]
            reachability_observation: None,
            relay_stall_state: RelayStallState::Healthy,
            relay_health: RelayHealthSnapshot {
                provider: "codex".to_string(),
                channel_id,
                active_turn: RelayActiveTurn::None,
                tmux_session: None,
                tmux_alive: None,
                watcher_attached: false,
                watcher_attached_stale: false,
                watcher_owner_channel_id: None,
                watcher_owns_live_relay: false,
                bridge_inflight_present: false,
                bridge_current_msg_id: None,
                mailbox_has_cancel_token: false,
                mailbox_active_user_msg_id: None,
                mailbox_turn_started_at_ms: None,
                mailbox_turn_age_secs: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: None,
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: None,
                last_relay_age_secs: None,
                last_outbound_activity_ms: None,
                last_capture_offset: None,
                last_relay_offset: 0,
                unread_bytes: None,
                desynced: false,
                stale_thread_proof: false,
                unpaired_active_token_reconfirmed: false,
            },
        }
    }

    #[test]
    fn destructive_consumers_cannot_read_reachability_for_routing() {
        let snapshot = include_str!("health/snapshot.rs");
        let production_snapshot = snapshot
            .split("#[cfg(test)]")
            .next()
            .expect("watcher snapshot production section");
        assert_eq!(
            production_snapshot
                .matches("reachability_observation:")
                .count(),
            1,
            "the snapshot has exactly one reachability observation field"
        );
        assert_eq!(
            production_snapshot
                .matches("reachability_observation,")
                .count(),
            1,
            "the snapshot constructor wires exactly one reachability observation"
        );
        assert_eq!(
            production_snapshot
                .matches("fn reachability_observation(")
                .count(),
            1,
            "the sole reachability-derived helper is the observation accessor"
        );
        assert_eq!(
            production_snapshot
                .matches("self.reachability_observation")
                .count(),
            1,
            "no authority-bearing alias helper may derive from the observation"
        );
        let watcher_snapshot_decl = production_snapshot
            .split("pub struct WatcherStateSnapshot")
            .next()
            .and_then(|prefix| prefix.rsplit("#[derive(").next())
            .expect("WatcherStateSnapshot derive");
        assert!(
            !watcher_snapshot_decl.contains("Debug"),
            "the reachability-bearing snapshot must not expose verdicts through Debug formatting"
        );
        let relay_recovery_source = include_str!("relay_recovery.rs");
        let production_relay_recovery = relay_recovery_source
            .split("#[cfg(all(test, unix))]")
            .next()
            .expect("axis-B production section");
        let static_names = production_relay_recovery
            .lines()
            .filter_map(|line| {
                let mut tokens = line.split_whitespace();
                tokens.find(|token| *token == "static")?;
                tokens.next().map(|name| name.trim_end_matches(':'))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            static_names,
            [
                "DESTRUCTIVE_CANCEL_POST_GATE_HOOK",
                "IDLE_TMUX_REATTACH_INFLIGHT_CANDIDATE_HOOK",
            ],
            "relay recovery may not acquire a new global side channel"
        );

        for (source, allowed_warrant_binds, fixture_observations, fixture_verdicts) in [
            (include_str!("health/recovery.rs"), 0, 1, 3),
            (
                include_str!("health/recovery/watchdog_decisions.rs"),
                1,
                0,
                0,
            ),
            (include_str!("router/intake_gate/stale_turn.rs"), 1, 3, 1),
            (include_str!("health/relay_auto_heal.rs"), 0, 1, 0),
            (include_str!("health/relay_dead_reattach.rs"), 0, 1, 0),
            (include_str!("relay_recovery/apply.rs"), 0, 0, 0),
        ] {
            assert_eq!(
                source.matches("reachability_observation").count(),
                fixture_observations,
                "only pinned test fixtures may name the raw reachability observation"
            );
            assert_eq!(
                source.matches("ReachabilityVerdict").count(),
                fixture_verdicts,
                "only the snapshot-binding fixture may construct a reachability verdict"
            );
            for forbidden in [
                "reachability_unknown_reason_label",
                "plan_relay_recovery_under_reachability",
                "axis_b_observation_report",
                "AxisBObservationReport",
                "AXIS_B_TRIAGE",
                "Debug::fmt",
                "format!(\"{snapshot:?}\")",
                "format!(\"{:?}\", snapshot)",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "a destructive consumer must not depend on axis-B observation via {forbidden}"
                );
            }
            assert_eq!(
                source.matches("structural_candidate_apply(").count(),
                allowed_warrant_binds,
                "each automatic destructive binding must mark its structural candidate"
            );
            assert_eq!(
                source.matches("destructive_warrant_bind(").count(),
                allowed_warrant_binds,
                "each marked structural candidate must bind the S6a warrant"
            );
            for routed in ["if observe_axis_b_candidate", "= observe_axis_b_candidate"] {
                assert!(
                    !source.contains(routed),
                    "observer result routed via {routed}"
                );
            }
        }
        assert_eq!(
            production_relay_recovery
                .matches(".reachability_observation")
                .count(),
            0,
            "recovery must not read the retired comparison operand"
        );
    }

    #[test]
    fn automatic_warrant_wiring_is_pinned_at_the_four_direct_consumers() {
        // These lexical wiring checks complement, not replace, the warrant behavior tests.
        let recovery = include_str!("relay_recovery.rs")
            .split("#[cfg(all(test, unix))]")
            .next()
            .unwrap();
        let body = |source: &str, symbol: &str| -> String {
            source
                .rsplit_once(&format!("fn {symbol}("))
                .expect("production function exists")
                .1
                .split_once("\n}")
                .expect("function end")
                .0
                .to_owned()
        };
        for (source, symbol) in [
            (recovery, "automatic_stale_sweep_warrants"),
            (recovery, "auto_apply_relay_recovery_for_shared_at"),
            (
                include_str!("health/recovery/watchdog_decisions.rs"),
                "watchdog_axis_b_warrants",
            ),
            (
                include_str!("router/intake_gate/stale_turn.rs"),
                "stale_turn_axis_b_warrants",
            ),
        ] {
            let owner = body(source, symbol);
            assert_eq!(
                owner.matches("observe_axis_b_candidate").count(),
                0,
                "automatic observation retired: {symbol}"
            );
            assert_eq!(
                owner.matches("destructive_warrant_bind(").count(),
                1,
                "automatic warrant retained: {symbol}"
            );
            assert!(
                owner.contains("destructive_warrant_bind.eligible"),
                "automatic warrant result consumed: {symbol}"
            );
        }
        assert_eq!(
            body(recovery, "run_relay_recovery")
                .matches("observe_axis_b_candidate(")
                .count(),
            0,
            "manual observation retired"
        );
        let apply = body(recovery, "auto_apply_relay_recovery_for_shared_at");
        let mapped = apply
            .split_once("if let Some(_site) = axis_b_site_for_apply(source, decision.action) {")
            .expect("mapped automatic guard retained")
            .1
            .split_once("} else if source != RelayRecoveryApplySource::Manual {")
            .expect("unmapped automatic deny retained")
            .0;
        assert!(mapped.contains("destructive_warrant::destructive_warrant_bind("));
        assert!(
            mapped.contains("decision.auto_heal.eligible = destructive_warrant_bind.eligible;")
        );
        for (source, needle) in [
            (
                include_str!("health/recovery.rs"),
                "AxisBSite::WatchdogStaleIdle",
            ),
            (
                include_str!("health/recovery.rs"),
                "AxisBSite::WatchdogExplicitBackground",
            ),
            (
                include_str!("router/intake_gate/stale_turn.rs"),
                "if !stale_turn_axis_b_warrants(shared, provider, &proof)",
            ),
            (
                include_str!("../../server/mod.rs"),
                "AxisBSite::PolicyTickStaleSweep",
            ),
            (
                include_str!("../../reconcile.rs"),
                "AxisBSite::BootReconcileSweep",
            ),
        ] {
            assert!(
                source.contains(needle),
                "automatic warrant wiring missing: {needle}"
            );
        }
    }

    #[test]
    fn axis_b_apply_site_mapping_separates_operator_and_automatic_taxonomy() {
        assert_eq!(
            axis_b_site_for_apply(
                RelayRecoveryApplySource::StallWatchdog,
                RelayRecoveryActionKind::ReattachWatcher,
            ),
            Some(AxisBSite::RelayDeadReattach)
        );
        assert_eq!(
            axis_b_site_for_apply(
                RelayRecoveryApplySource::ProbeAutoHeal,
                RelayRecoveryActionKind::ClearOrphanPendingToken,
            ),
            Some(AxisBSite::ProbeAutoHeal)
        );
        assert_eq!(
            axis_b_site_for_apply(
                RelayRecoveryApplySource::StallWatchdog,
                RelayRecoveryActionKind::ClearOrphanPendingToken,
            ),
            Some(AxisBSite::WatchdogExplicitBackground)
        );
        assert_eq!(
            axis_b_site_for_apply(
                RelayRecoveryApplySource::StallWatchdog,
                RelayRecoveryActionKind::ClearStaleThreadProof,
            ),
            Some(AxisBSite::WatchdogStaleIdle)
        );
        assert_eq!(
            axis_b_site_for_apply(
                RelayRecoveryApplySource::StallWatchdog,
                RelayRecoveryActionKind::DrainPendingQueue,
            ),
            None,
            "the stall-watchdog apply surface has no pending-queue drain lane"
        );
        assert_eq!(
            axis_b_site_for_apply(
                RelayRecoveryApplySource::ProbeAutoHeal,
                RelayRecoveryActionKind::ReattachWatcher,
            ),
            Some(AxisBSite::ProbeAutoHealReattach),
            "redrive reattach is a plan_relay_recovery consumer in design §3.4"
        );
        for action in [
            RelayRecoveryActionKind::ClearStaleThreadProof,
            RelayRecoveryActionKind::DrainPendingQueue,
        ] {
            assert_eq!(
                axis_b_site_for_apply(RelayRecoveryApplySource::ProbeAutoHeal, action),
                Some(AxisBSite::ProbeAutoHeal),
                "every structural-planner destructive action must map to a warrant site"
            );
        }
        for source in [
            RelayRecoveryApplySource::ProbeAutoHeal,
            RelayRecoveryApplySource::StallWatchdog,
        ] {
            assert!(pinned_adoption_for_apply(
                source,
                RelayRecoveryActionKind::ReattachWatcher,
            ));
            assert!(!pinned_adoption_for_apply(
                source,
                RelayRecoveryActionKind::ClearOrphanPendingToken,
            ));
        }
        assert!(!pinned_adoption_for_apply(
            RelayRecoveryApplySource::Manual,
            RelayRecoveryActionKind::ReattachWatcher,
        ));
    }

    #[test]
    fn automatic_reattach_sources_keep_transport_unknown_recovery_eligible() {
        use health::reachability::verdict::{ReachabilityVerdict, TransportUnknownEvidence};

        let temp = tempfile::tempdir().expect("axis-B temp root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let provider = ProviderKind::Codex;
        let mut snapshot = quiet_snapshot_for_warrant_tests(54_643);
        snapshot.reachability_observation = Some((
            ReachabilityVerdict::TransportUnknown {
                since_secs: 700,
                evidence: TransportUnknownEvidence::UnreleasedDeliveryLease,
            },
            900,
        ));

        for source in [
            RelayRecoveryApplySource::ProbeAutoHeal,
            RelayRecoveryApplySource::StallWatchdog,
        ] {
            let pinned_adoption =
                pinned_adoption_for_apply(source, RelayRecoveryActionKind::ReattachWatcher);
            assert!(
                pinned_adoption,
                "automatic reattach source {source:?} must derive pinned adoption from its durable episode reservation"
            );
            assert!(
                destructive_warrant::destructive_warrant_bind(
                    true,
                    RelayRecoveryActionKind::ReattachWatcher,
                    &provider,
                    Some(&snapshot),
                    pinned_adoption,
                )
                .eligible,
                "TransportUnknown must not stop {source:?} pinned reattach recovery"
            );
        }
    }
}

#[cfg(test)]
#[path = "relay_recovery/tests.rs"]
mod tests;
