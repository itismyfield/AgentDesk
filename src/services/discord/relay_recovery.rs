//! Relay recovery dry-run planner and conservative auto-heal executor.
//!
//! This module is intentionally narrow: it turns the read-only relay health
//! classifier into an operator-facing decision, and only applies local,
//! idempotent cleanup when the evidence is strong enough.
//!
//! Known residual limitations for follow-up issues: a committed-but-leaked
//! inflight row self-heals only when a pending-start backstop can prove the
//! terminal envelope and complete it through the finalizer; relay recovery still
//! has no independent sweep for that shape. Rows whose `output_path` is missing
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
    SharedData, mailbox_clear_channel, mailbox_clear_recovery_marker, mailbox_finish_turn,
    mailbox_snapshot,
};
use crate::services::provider::ProviderKind;

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
#[path = "relay_recovery_reattach_apply.rs"]
mod reattach_apply;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum RelayRecoveryActionKind {
    ObserveOnly,
    ClearStaleThreadProof,
    ClearOrphanPendingToken,
    ReattachWatcher,
    DrainPendingQueue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum RelayRecoveryApplySource {
    Manual,
    ProbeAutoHeal,
    StallWatchdog,
}

impl RelayRecoveryApplySource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::ProbeAutoHeal => "probe_auto_heal",
            Self::StallWatchdog => "stall_watchdog",
        }
    }

    fn finalizer_reason(self) -> &'static str {
        match self {
            Self::StallWatchdog => "1446_stall_watchdog",
            Self::Manual | Self::ProbeAutoHeal => "1462_relay_recovery_auto_heal",
        }
    }

    fn cleanup_session(self) -> bool {
        matches!(self, Self::StallWatchdog)
    }
}

impl RelayRecoveryActionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::ObserveOnly => "observe_only",
            Self::ClearStaleThreadProof => "clear_stale_thread_proof",
            Self::ClearOrphanPendingToken => "clear_orphan_pending_token",
            Self::ReattachWatcher => "reattach_watcher",
            Self::DrainPendingQueue => "drain_pending_queue",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::services::discord) struct RelayRecoveryEvidence {
    pub active_turn: RelayActiveTurn,
    pub tmux_session: Option<String>,
    pub tmux_alive: Option<bool>,
    pub watcher_attached: bool,
    pub watcher_owner_channel_id: Option<u64>,
    pub watcher_owns_live_relay: bool,
    pub bridge_inflight_present: bool,
    pub mailbox_has_cancel_token: bool,
    pub mailbox_active_user_msg_id: Option<u64>,
    pub queue_depth: usize,
    pub pending_thread_proof: bool,
    pub stale_thread_proof: bool,
    pub desynced: bool,
    pub last_capture_offset: Option<u64>,
    pub last_relay_offset: u64,
    pub last_relay_ts_ms: Option<i64>,
    pub unread_bytes: Option<u64>,
    pub last_outbound_activity_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::services::discord) struct RelayRecoveryAffectedIdentifiers {
    pub provider: String,
    pub channel_id: u64,
    pub parent_channel_id: Option<u64>,
    pub thread_channel_id: Option<u64>,
    pub tmux_session: Option<String>,
    pub mailbox_active_user_msg_id: Option<u64>,
    pub bridge_current_msg_id: Option<u64>,
    pub finalizer_turn_id: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::services::discord) struct RelayRecoveryAutoHeal {
    pub eligible: bool,
    pub bounded: bool,
    pub max_attempts_per_window: u32,
    pub window_secs: i64,
    pub remaining_attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<&'static str>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::services::discord) struct RelayRecoveryDecision {
    pub provider: String,
    pub channel_id: u64,
    pub relay_stall_state: RelayStallState,
    pub action: RelayRecoveryActionKind,
    pub reason: &'static str,
    pub evidence: RelayRecoveryEvidence,
    pub affected: RelayRecoveryAffectedIdentifiers,
    pub auto_heal: RelayRecoveryAutoHeal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::services::discord) struct RelayRecoveryApplyResult {
    pub status: &'static str,
    pub removed_thread_proofs: usize,
    pub removed_mailbox_token: bool,
    pub post_mailbox_has_cancel_token: Option<bool>,
    pub post_mailbox_queue_depth: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reattach_watcher_spawned: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reattach_watcher_replaced: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reattach_initial_offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reattach_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::services::discord) struct RelayRecoveryResponse {
    pub ok: bool,
    pub mode: &'static str,
    pub applied: bool,
    pub skipped: bool,
    pub decision: RelayRecoveryDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apply_result: Option<RelayRecoveryApplyResult>,
}

#[derive(Debug)]
pub(in crate::services::discord) enum RelayRecoveryError {
    InvalidProvider(String),
    SnapshotNotFound {
        channel_id: u64,
        provider: Option<String>,
    },
    ProviderUnavailable(String),
}

impl RelayRecoveryError {
    pub(in crate::services::discord) fn status_str(&self) -> &'static str {
        match self {
            Self::InvalidProvider(_) => "400 Bad Request",
            Self::SnapshotNotFound { .. } => "404 Not Found",
            Self::ProviderUnavailable(_) => "503 Service Unavailable",
        }
    }

    pub(in crate::services::discord) fn body(&self) -> serde_json::Value {
        match self {
            Self::InvalidProvider(provider) => serde_json::json!({
                "ok": false,
                "error": "invalid provider",
                "provider": provider
            }),
            Self::SnapshotNotFound {
                channel_id,
                provider,
            } => serde_json::json!({
                "ok": false,
                "error": "no relay health snapshot for channel",
                "channel_id": channel_id,
                "provider": provider
            }),
            Self::ProviderUnavailable(provider) => serde_json::json!({
                "ok": false,
                "error": "provider runtime unavailable",
                "provider": provider
            }),
        }
    }
}

fn is_agentdesk_tmux_session(tmux_session: Option<&str>) -> bool {
    tmux_session.is_some_and(|session| session.starts_with("AgentDesk-"))
}

fn evidence_from_snapshot(snapshot: &RelayHealthSnapshot) -> RelayRecoveryEvidence {
    RelayRecoveryEvidence {
        active_turn: snapshot.active_turn,
        tmux_session: snapshot.tmux_session.clone(),
        tmux_alive: snapshot.tmux_alive,
        watcher_attached: snapshot.watcher_attached,
        watcher_owner_channel_id: snapshot.watcher_owner_channel_id,
        watcher_owns_live_relay: snapshot.watcher_owns_live_relay,
        bridge_inflight_present: snapshot.bridge_inflight_present,
        mailbox_has_cancel_token: snapshot.mailbox_has_cancel_token,
        mailbox_active_user_msg_id: snapshot.mailbox_active_user_msg_id,
        queue_depth: snapshot.queue_depth,
        pending_thread_proof: snapshot.pending_thread_proof,
        stale_thread_proof: snapshot.stale_thread_proof,
        desynced: snapshot.desynced,
        last_capture_offset: snapshot.last_capture_offset,
        last_relay_offset: snapshot.last_relay_offset,
        last_relay_ts_ms: snapshot.last_relay_ts_ms,
        unread_bytes: snapshot.unread_bytes,
        last_outbound_activity_ms: snapshot.last_outbound_activity_ms,
    }
}

fn affected_from_snapshot(snapshot: &RelayHealthSnapshot) -> RelayRecoveryAffectedIdentifiers {
    RelayRecoveryAffectedIdentifiers {
        provider: snapshot.provider.clone(),
        channel_id: snapshot.channel_id,
        parent_channel_id: snapshot.parent_channel_id,
        thread_channel_id: snapshot.thread_channel_id,
        tmux_session: snapshot.tmux_session.clone(),
        mailbox_active_user_msg_id: snapshot.mailbox_active_user_msg_id,
        bridge_current_msg_id: snapshot.bridge_current_msg_id,
        finalizer_turn_id: None,
    }
}

fn eligible_stale_thread_proof(snapshot: &RelayHealthSnapshot) -> bool {
    snapshot.pending_thread_proof
        && snapshot.stale_thread_proof
        && !snapshot.mailbox_has_cancel_token
        && !snapshot.bridge_inflight_present
        && !snapshot.watcher_attached
        && snapshot.tmux_alive != Some(true)
}

fn eligible_orphan_pending_token(snapshot: &RelayHealthSnapshot) -> bool {
    snapshot.mailbox_has_cancel_token
        && !snapshot.bridge_inflight_present
        && !snapshot.watcher_attached
        && snapshot.tmux_alive != Some(true)
        && !is_agentdesk_tmux_session(snapshot.tmux_session.as_deref())
}

fn eligible_reattach_watcher(snapshot: &RelayHealthSnapshot) -> bool {
    // #3277 (Defect D): a watcher binding whose heartbeat is stale
    // (`watcher_attached_stale`) must not block bounded reattach the way a
    // genuinely-live watcher does. A fresh-heartbeat live watcher still makes
    // this ineligible: auto-heal never replaces a live handle (that case is the
    // finalizer far-backstop's job, #3277 Defect C). Cancelled handles are
    // replaced by the watcher claim path, not mislabeled as heartbeat-stale.
    //
    // A mailbox token is strong live-turn evidence, but it is not required for
    // post-restart adoption: a valid inflight row can outlive the in-memory
    // mailbox token while the AgentDesk tmux session keeps producing output.
    // In that inflight-only shape, allow bounded reattach when there is no
    // competing mailbox owner.
    snapshot.tmux_alive == Some(true)
        && snapshot.bridge_inflight_present
        && (snapshot.mailbox_has_cancel_token || snapshot.mailbox_active_user_msg_id.is_none())
        && (!snapshot.watcher_attached
            || snapshot.watcher_attached_stale
            || !snapshot.watcher_owns_live_relay
            || snapshot.relay_frontier_never_advanced_with_unread_tail())
        && snapshot.desynced
        && is_agentdesk_tmux_session(snapshot.tmux_session.as_deref())
}

fn auto_heal_metadata(
    snapshot: &RelayHealthSnapshot,
    action: RelayRecoveryActionKind,
    eligible: bool,
    skipped_reason: Option<&'static str>,
    now_ms: i64,
) -> RelayRecoveryAutoHeal {
    let key = auto_heal_key(
        &snapshot.provider,
        snapshot.channel_id,
        action,
        RelayRecoveryApplySource::Manual,
    );
    let max_attempts_per_window = max_attempts_per_window_for_snapshot(snapshot, action);
    RelayRecoveryAutoHeal {
        eligible,
        bounded: true,
        max_attempts_per_window,
        window_secs: AUTO_HEAL_WINDOW_SECS,
        remaining_attempts: remaining_auto_heal_attempts(&key, now_ms, max_attempts_per_window),
        skipped_reason,
    }
}

pub(in crate::services::discord) fn plan_relay_recovery(
    snapshot: &RelayHealthSnapshot,
    relay_stall_state: RelayStallState,
    now_ms: i64,
) -> RelayRecoveryDecision {
    let protected_tmux = is_agentdesk_tmux_session(snapshot.tmux_session.as_deref());
    let (action, reason, eligible, skipped_reason) = match relay_stall_state {
        RelayStallState::Healthy => (
            RelayRecoveryActionKind::ObserveOnly,
            "relay is healthy",
            false,
            Some("no_recovery_needed"),
        ),
        RelayStallState::ActiveForegroundStream => (
            RelayRecoveryActionKind::ObserveOnly,
            "foreground stream has live turn evidence",
            false,
            Some("live_foreground_turn"),
        ),
        RelayStallState::ExplicitBackgroundWork => (
            RelayRecoveryActionKind::ObserveOnly,
            "explicit background work is allowed to stay quiet",
            false,
            Some("explicit_background_work"),
        ),
        RelayStallState::TmuxAliveRelayDead => {
            let eligible = eligible_reattach_watcher(snapshot);
            (
                RelayRecoveryActionKind::ReattachWatcher,
                if eligible {
                    "tmux is alive but relay watcher is detached; bounded reattach can restore delivery"
                } else {
                    "tmux is alive but relay state is desynced; reattach requires explicit operator flow"
                },
                eligible,
                (!eligible).then_some(if protected_tmux {
                    "reattach_missing_required_live_evidence"
                } else {
                    "reattach_requires_explicit_rebind"
                }),
            )
        }
        RelayStallState::StaleThreadProof => {
            let eligible = eligible_stale_thread_proof(snapshot);
            (
                RelayRecoveryActionKind::ClearStaleThreadProof,
                "thread proof exists without live child relay evidence",
                eligible,
                (!eligible).then_some("stale_thread_proof_has_live_evidence"),
            )
        }
        RelayStallState::OrphanPendingToken => {
            let eligible = eligible_orphan_pending_token(snapshot);
            (
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                "mailbox holds a cancel token without bridge, watcher, or live tmux evidence",
                eligible,
                (!eligible).then_some(if protected_tmux {
                    "protected_agentdesk_tmux_session"
                } else {
                    "orphan_token_has_live_evidence"
                }),
            )
        }
        RelayStallState::QueueBlocked => {
            let eligible = matches!(snapshot.active_turn, RelayActiveTurn::None)
                && !snapshot.mailbox_has_cancel_token
                && snapshot.mailbox_active_user_msg_id.is_none();
            (
                RelayRecoveryActionKind::DrainPendingQueue,
                if eligible {
                    "queued work is stranded behind an idle mailbox; bounded queue drain can restore delivery"
                } else {
                    "queued work exists but live turn evidence prevents automatic queue drain"
                },
                eligible,
                (!eligible).then_some("queue_blocked_has_live_turn_evidence"),
            )
        }
    };

    RelayRecoveryDecision {
        provider: snapshot.provider.clone(),
        channel_id: snapshot.channel_id,
        relay_stall_state,
        action,
        reason,
        evidence: evidence_from_snapshot(snapshot),
        affected: affected_from_snapshot(snapshot),
        auto_heal: auto_heal_metadata(snapshot, action, eligible, skipped_reason, now_ms),
    }
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
    let snapshot = registry
        .snapshot_watcher_state_for_shared(provider, shared.clone(), channel_id)
        .await
        .ok_or_else(|| RelayRecoveryError::SnapshotNotFound {
            channel_id,
            provider: Some(provider.as_str().to_string()),
        })?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut decision =
        plan_relay_recovery(&snapshot.relay_health, snapshot.relay_stall_state, now_ms);
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

#[derive(Clone, Debug)]
struct RelayRecoveryInflightClearPin {
    identity: super::inflight::InflightTurnIdentity,
    finalizer_turn_id: u64,
    updated_at: String,
    save_generation: u64,
}

fn load_idle_tmux_reattach_inflight_clear_candidate(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<super::inflight::InflightTurnState> {
    let state = super::inflight::load_inflight_state(provider, channel_id)?;
    if !super::inflight::inflight_state_allows_idle_tmux_repair_state(&state) {
        return None;
    }
    #[cfg(test)]
    if let Some(hook) = idle_tmux_reattach_inflight_candidate_hook()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
    {
        hook(&state);
    }
    Some(state)
}

fn capture_idle_tmux_reattach_inflight_clear_pin(
    state: &super::inflight::InflightTurnState,
) -> Option<RelayRecoveryInflightClearPin> {
    let finalizer_turn_id = state.effective_finalizer_turn_id();
    (finalizer_turn_id != 0).then(|| RelayRecoveryInflightClearPin {
        identity: super::inflight::InflightTurnIdentity::from_state(state),
        finalizer_turn_id,
        updated_at: state.updated_at.clone(),
        save_generation: state.save_generation,
    })
}

fn clear_idle_tmux_reattach_inflight_if_pinned(
    provider: &ProviderKind,
    channel_id: u64,
    pin: Option<&RelayRecoveryInflightClearPin>,
) -> super::inflight::GuardedClearOutcome {
    let Some(pin) = pin else {
        return super::inflight::GuardedClearOutcome::Missing;
    };
    let outcome = super::inflight::clear_inflight_state_if_matches_identity_generation(
        provider,
        channel_id,
        &pin.identity,
        pin.finalizer_turn_id,
        &pin.updated_at,
        pin.save_generation,
    );
    match outcome {
        super::inflight::GuardedClearOutcome::Cleared
        | super::inflight::GuardedClearOutcome::Missing => {}
        other => warn_idle_tmux_reattach_inflight_clear_refused(provider, channel_id, pin, other),
    }
    outcome
}

fn warn_idle_tmux_reattach_inflight_clear_refused(
    provider: &ProviderKind,
    channel_id: u64,
    pin: &RelayRecoveryInflightClearPin,
    outcome: super::inflight::GuardedClearOutcome,
) {
    let current = super::inflight::load_inflight_state(provider, channel_id);
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id,
        clear_outcome = ?outcome,
        expected_user_msg_id = pin.identity.user_msg_id,
        expected_finalizer_turn_id = pin.finalizer_turn_id,
        expected_updated_at = %pin.updated_at,
        expected_save_generation = pin.save_generation,
        current_user_msg_id = current.as_ref().map(|state| state.user_msg_id).unwrap_or(0),
        current_finalizer_turn_id = current
            .as_ref()
            .map(|state| state.effective_finalizer_turn_id())
            .unwrap_or(0),
        current_updated_at = %current
            .as_ref()
            .map(|state| state.updated_at.as_str())
            .unwrap_or("<missing>"),
        current_save_generation = current.as_ref().map(|state| state.save_generation).unwrap_or(0),
        "idle tmux stale-turn repair skipped persistent inflight clear because the readiness-time pin no longer matches"
    );
}

fn idle_tmux_reattach_clear_status(outcome: super::inflight::GuardedClearOutcome) -> &'static str {
    match outcome {
        super::inflight::GuardedClearOutcome::Cleared => "cleared_idle_tmux_stale_turn",
        super::inflight::GuardedClearOutcome::IoError => "skipped_idle_tmux_stale_turn_io_error",
        super::inflight::GuardedClearOutcome::Missing => "skipped_idle_tmux_stale_turn_missing",
        super::inflight::GuardedClearOutcome::UserMsgMismatch
        | super::inflight::GuardedClearOutcome::PlannedRestartSkipped
        | super::inflight::GuardedClearOutcome::RebindOriginSkipped => {
            "skipped_idle_tmux_stale_turn_pin_mismatch"
        }
    }
}

fn relay_recovery_cancel_finalize_context() -> super::turn_finalizer::FinalizeContext {
    super::turn_finalizer::FinalizeContext {
        clear_inflight: true,
        allow_completion_cleanup: false,
        drain_voice: false,
        kickoff_queue: true,
        expected_idempotent_guard_miss: false,
    }
}

fn relay_recovery_destructive_cancel_pin(
    decision: &RelayRecoveryDecision,
) -> Option<super::destructive_cancel_gate::DestructiveCancelIdentityPin> {
    Some(
        super::destructive_cancel_gate::DestructiveCancelIdentityPin {
            finalizer_turn_id: decision.affected.finalizer_turn_id?,
            mailbox_active_user_msg_id: decision.affected.mailbox_active_user_msg_id,
            tmux_session_name: decision.affected.tmux_session.clone(),
        },
    )
}

fn relay_recovery_probe_snapshot_for_owner(
    shared: &super::SharedData,
    provider: &ProviderKind,
    owner_channel_id: ChannelId,
    decision: &RelayRecoveryDecision,
) -> Result<super::destructive_cancel_gate::DestructiveCancelProbeSnapshot, &'static str> {
    let Some(pin) = relay_recovery_destructive_cancel_pin(decision) else {
        return Err("missing_decision_identity_pin");
    };
    let Some(state) = super::inflight::load_inflight_state(provider, owner_channel_id.get()) else {
        return Err("inflight_missing_before_cancel");
    };
    if !pin.matches_state(&state) {
        return Err("identity_mismatch_before_cancel");
    }
    Ok(
        super::destructive_cancel_gate::DestructiveCancelProbeSnapshot::from_pinned_state(
            shared,
            &state,
            pin,
            owner_channel_id,
        ),
    )
}

async fn finalize_cancelled_watcher_owner_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    decision: &RelayRecoveryDecision,
    owner_channel_id: ChannelId,
) -> Option<super::turn_finalizer::FinalizeOutcome> {
    let finalizer_turn_id = decision.affected.finalizer_turn_id?;
    if finalizer_turn_id == 0 {
        return None;
    }
    Some(
        shared
            .turn_finalizer
            .submit_terminal(
                super::turn_finalizer::TurnKey::new(
                    owner_channel_id,
                    finalizer_turn_id,
                    shared.restart.current_generation,
                ),
                provider.clone(),
                super::turn_finalizer::TerminalEvent::Cancel,
                relay_recovery_cancel_finalize_context(),
                shared.clone(),
            )
            .await,
    )
}

pub(in crate::services::discord) fn idle_tmux_repair_ready_for_input(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session: &str,
) -> bool {
    idle_tmux_repair_ready_for_input_with_pane_probe(
        provider,
        channel_id,
        tmux_session,
        idle_tmux_repair_pane_ready_for_input,
    )
}

pub(in crate::services::discord) fn idle_tmux_repair_state_ready_for_input(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session: &str,
    state: &super::inflight::InflightTurnState,
) -> bool {
    idle_tmux_repair_snapshot_ready_for_input(
        provider,
        channel_id,
        tmux_session,
        state,
        idle_tmux_repair_pane_ready_for_input,
    )
}

fn idle_tmux_repair_pane_ready_for_input(tmux_session: &str, provider: &ProviderKind) -> bool {
    // Pre-existing recovery override for long-frozen Busy JSONL. This is
    // intentionally not `FallbackPaneReadiness`: the override is scoped by
    // `frozen_busy_jsonl_allows_pane_fallback` below.
    crate::services::platform::tmux::capture_pane(tmux_session, -80)
        .map(|pane| {
            crate::services::provider::tmux_capture_indicates_ready_for_input(&pane, provider)
        })
        .unwrap_or(false)
}

fn idle_tmux_repair_ready_for_input_with_pane_probe(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session: &str,
    pane_ready_for_input: impl Fn(&str, &ProviderKind) -> bool,
) -> bool {
    let Some(state) = super::inflight::load_inflight_state(provider, channel_id) else {
        return pane_ready_for_input(tmux_session, provider);
    };
    idle_tmux_repair_snapshot_ready_for_input(
        provider,
        channel_id,
        tmux_session,
        &state,
        pane_ready_for_input,
    )
}

fn idle_tmux_repair_snapshot_ready_for_input(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session: &str,
    state: &super::inflight::InflightTurnState,
    pane_ready_for_input: impl Fn(&str, &ProviderKind) -> bool,
) -> bool {
    let Some(output_path) = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return pane_ready_for_input(tmux_session, provider);
    };
    let output_path = Path::new(output_path);
    let Some(structured_ready) = crate::services::tui_turn_state::jsonl_ready_for_input(
        provider,
        state.runtime_kind,
        output_path,
        Some(state.last_offset),
    ) else {
        return pane_ready_for_input(tmux_session, provider);
    };

    match structured_ready {
        crate::services::tui_turn_state::TuiReadyState::Ready => true,
        crate::services::tui_turn_state::TuiReadyState::Busy
            if frozen_busy_jsonl_allows_pane_fallback(output_path) =>
        {
            let pane_ready = pane_ready_for_input(tmux_session, provider);
            if pane_ready {
                tracing::warn!(
                    target: "agentdesk::discord::relay_recovery",
                    provider = provider.as_str(),
                    channel_id,
                    tmux_session,
                    output_path = %output_path.display(),
                    stale_secs = FROZEN_BUSY_JSONL_READY_FALLBACK_AGE.as_secs(),
                    "idle-tmux repair accepted pane-ready fallback for frozen Busy JSONL"
                );
            }
            pane_ready
        }
        crate::services::tui_turn_state::TuiReadyState::Busy
        | crate::services::tui_turn_state::TuiReadyState::Unknown => false,
    }
}

fn frozen_busy_jsonl_allows_pane_fallback(output_path: &Path) -> bool {
    output_file_quiescent_for_duration(output_path, FROZEN_BUSY_JSONL_READY_FALLBACK_AGE)
}

fn output_file_quiescent_for_duration(output_path: &Path, min_age: Duration) -> bool {
    output_file_quiescent_for_duration_at(output_path, min_age, SystemTime::now())
}

fn output_file_quiescent_for_duration_at(
    output_path: &Path,
    min_age: Duration,
    now: SystemTime,
) -> bool {
    let Ok(metadata) = std::fs::metadata(output_path) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() == 0 {
        return false;
    }
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    now.duration_since(modified).is_ok_and(|age| age >= min_age)
}

/// Channel-scoped entry for callers outside the `discord` subtree (e.g. the
/// manual stale-mailbox repair route) that cannot reach the `pub(super)`
/// inflight loader: loads the current row and delegates to the state-based
/// guard below. Absent row → no tail answer to lose → false.
pub(crate) fn channel_has_unrelayed_idle_tmux_tail_answer(
    provider: &ProviderKind,
    channel_id: u64,
) -> bool {
    super::inflight::load_inflight_state(provider, channel_id)
        .is_some_and(|state| idle_tmux_repair_has_unrelayed_tail_answer(&state))
}

/// #3668 F2: detect tail answer text that the destructive idle-tmux clear would
/// permanently lose.
///
/// `idle_tmux_repair_ready_for_input` returns Ready when the JSONL has a
/// terminal envelope after `last_offset` (the offset-behind path in
/// `tui_turn_state::jsonl_ready_for_input`), which means a final answer is
/// already persisted past the inflight watermark. The companion inflight guard
/// (`inflight_state_allows_idle_tmux_repair`) only inspects the streaming
/// `full_response`, so an empty-stream + JSONL-terminal-answer row passes both
/// guards and reaches `clear_inflight_state`, dropping text that
/// `extract_response_from_output_pub(output_path, last_offset)` could still
/// recover. The recovery_engine normal path (extract → relay → clear) never has
/// this asymmetry. This guard reads the same offset slice read-only: if it
/// yields non-empty relayable text, the caller skips the destructive clear and
/// falls through to the non-destructive rebind path (which preserves the
/// inflight/output so normal relay/recovery delivers the text). On extract
/// failure / IO error the function returns false → existing behavior (only the
/// genuinely-empty tail still clears), so this is behavior-preserving.
pub(crate) fn idle_tmux_repair_has_unrelayed_tail_answer(
    state: &super::inflight::InflightTurnState,
) -> bool {
    let Some(output_path) = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return false;
    };
    // #3668 codex r3: only treat this as an answer worth preserving when there
    // is TERMINAL completion evidence — a *successful* `result` record after
    // `last_offset`. A hung / desynced turn with only partial assistant text and
    // no terminal result must NOT suppress the destructive idle-clear / force-
    // clean: otherwise the watchdog would skip it every tick forever, since
    // #3645 far-backstop / normal recovery only advance `last_offset` on a
    // terminal success. Requiring the success-result record keeps the guard to
    // genuinely-deliverable, complete-but-unrelayed answers.
    if super::recovery::success_result_end_offset_after_offset(output_path, state.last_offset)
        .is_none()
    {
        return false;
    }
    let tail = super::recovery::extract_response_from_output_pub(output_path, state.last_offset);
    !tail.trim().is_empty()
}

async fn apply_relay_recovery_decision(
    registry: &HealthRegistry,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    decision: &RelayRecoveryDecision,
    episode: Option<&circuit_breaker::RelayReattachEpisode>,
    source: RelayRecoveryApplySource,
) -> RelayRecoveryApplyResult {
    match decision.action {
        RelayRecoveryActionKind::ClearStaleThreadProof => {
            let channel = ChannelId::new(decision.channel_id);
            let before = shared.dispatch.thread_parents.len();
            let mut removed_parents = Vec::new();
            shared.dispatch.thread_parents.retain(|parent, thread| {
                let remove = *parent == channel || *thread == channel;
                if remove {
                    removed_parents.push(*parent);
                }
                !remove
            });
            super::turn_finalizer::cleanup::kickoff_thread_parents_after_finalize(
                shared,
                provider,
                removed_parents,
            );
            RelayRecoveryApplyResult {
                status: "applied",
                removed_thread_proofs: before.saturating_sub(shared.dispatch.thread_parents.len()),
                removed_mailbox_token: false,
                post_mailbox_has_cancel_token: None,
                post_mailbox_queue_depth: None,
                reattach_watcher_spawned: None,
                reattach_watcher_replaced: None,
                reattach_initial_offset: None,
                reattach_error: None,
            }
        }
        RelayRecoveryActionKind::ClearOrphanPendingToken => {
            let channel = ChannelId::new(decision.channel_id);
            let cleared = mailbox_clear_channel(shared, provider, channel).await;
            if source.cleanup_session() {
                super::stall_recovery::finalize_orphaned_clear(
                    shared,
                    channel,
                    cleared.removed_token.clone(),
                    source.finalizer_reason(),
                );
            } else {
                super::stall_recovery::finalize_orphaned_clear_preserve_session(
                    shared,
                    channel,
                    cleared.removed_token.clone(),
                    source.finalizer_reason(),
                );
            }
            mailbox_clear_recovery_marker(shared, channel).await;
            let after = mailbox_snapshot(shared, channel).await;
            RelayRecoveryApplyResult {
                status: "applied",
                removed_thread_proofs: 0,
                removed_mailbox_token: cleared.removed_token.is_some(),
                post_mailbox_has_cancel_token: Some(after.cancel_token.is_some()),
                post_mailbox_queue_depth: Some(after.intervention_queue.len()),
                reattach_watcher_spawned: None,
                reattach_watcher_replaced: None,
                reattach_initial_offset: None,
                reattach_error: None,
            }
        }
        RelayRecoveryActionKind::ReattachWatcher => {
            let channel = ChannelId::new(decision.channel_id);
            // The durable automatic lane is deliberately non-destructive: its
            // exact episode is adopted by `rebind_inflight` below.  The legacy
            // manual lane keeps the idle-turn retirement behavior.
            if episode.is_none()
                && let Some(tmux_session) = decision.affected.tmux_session.as_deref()
                && decision.evidence.unread_bytes.unwrap_or(0) == 0
                // This branch intentionally does not route through
                // `destructive_cancel_gate`: the snapshot readiness check is
                // the turn-scope proof that the provider prompt has returned
                // (structured JSONL ready state, or tmux prompt fallback), and the
                // following inflight/tail guards prove there is no deliverable
                // assistant body left to preserve. The cleanup below only retires
                // stale mailbox/inflight bookkeeping for an already-idle turn.
                && let Some(inflight_clear_state) =
                    load_idle_tmux_reattach_inflight_clear_candidate(provider, decision.channel_id)
                && idle_tmux_repair_snapshot_ready_for_input(
                    provider,
                    decision.channel_id,
                    tmux_session,
                    &inflight_clear_state,
                    idle_tmux_repair_pane_ready_for_input,
                )
                // #3668 F2: never destructively clear when a final answer is
                // still persisted in JSONL after `last_offset` — fall through to
                // the non-destructive rebind path so normal relay delivers it.
                && !idle_tmux_repair_has_unrelayed_tail_answer(&inflight_clear_state)
            {
                let inflight_clear_pin =
                    capture_idle_tmux_reattach_inflight_clear_pin(&inflight_clear_state);
                let inflight_clear_outcome = clear_idle_tmux_reattach_inflight_if_pinned(
                    provider,
                    decision.channel_id,
                    inflight_clear_pin.as_ref(),
                );
                if !matches!(
                    inflight_clear_outcome,
                    super::inflight::GuardedClearOutcome::Cleared
                ) {
                    let after = mailbox_snapshot(shared, channel).await;
                    return RelayRecoveryApplyResult {
                        status: idle_tmux_reattach_clear_status(inflight_clear_outcome),
                        removed_thread_proofs: 0,
                        removed_mailbox_token: false,
                        post_mailbox_has_cancel_token: Some(after.cancel_token.is_some()),
                        post_mailbox_queue_depth: Some(after.intervention_queue.len()),
                        reattach_watcher_spawned: Some(false),
                        reattach_watcher_replaced: Some(false),
                        reattach_initial_offset: None,
                        reattach_error: None,
                    };
                }
                completion_footer::forget_if_message(
                    channel,
                    decision.affected.bridge_current_msg_id,
                );
                if let Some((_, watcher)) = shared.tmux_watchers.remove(&channel) {
                    watcher.cancel.store(true, Ordering::Relaxed);
                }
                // #4198: snapshot before the yielding finish/cleanup awaits so
                // the remove below cannot clobber a same-channel follow-up's
                // freshly inserted override.
                let owned_role_override =
                    super::turn_finalizer::cleanup::snapshot_role_override(shared, channel);
                let finish = mailbox_finish_turn(shared, provider, channel).await;
                if let Some(token) = finish.removed_token.as_ref() {
                    token.cancelled.store(true, Ordering::Relaxed);
                    super::saturating_decrement_global_active(shared);
                }
                super::clear_watchdog_deadline_override(channel.get()).await;
                let thread_parent_kickoffs =
                    super::turn_finalizer::cleanup::collect_and_clear_thread_parents(
                        shared, channel,
                    );
                super::turn_finalizer::cleanup::kickoff_thread_parents_after_finalize(
                    shared,
                    provider,
                    thread_parent_kickoffs,
                );
                shared.restart.recovering_channels.remove(&channel);
                shared.turn_start_times.remove(&channel);
                if !finish.has_pending {
                    super::turn_finalizer::cleanup::remove_owned_role_override(
                        shared,
                        channel,
                        owned_role_override,
                    );
                }
                mailbox_clear_recovery_marker(shared, channel).await;
                let after = mailbox_snapshot(shared, channel).await;
                return RelayRecoveryApplyResult {
                    status: idle_tmux_reattach_clear_status(inflight_clear_outcome),
                    removed_thread_proofs: 0,
                    removed_mailbox_token: finish.removed_token.is_some(),
                    post_mailbox_has_cancel_token: Some(after.cancel_token.is_some()),
                    post_mailbox_queue_depth: Some(after.intervention_queue.len()),
                    reattach_watcher_spawned: Some(false),
                    reattach_watcher_replaced: Some(matches!(
                        inflight_clear_outcome,
                        super::inflight::GuardedClearOutcome::Cleared
                    )),
                    reattach_initial_offset: None,
                    reattach_error: None,
                };
            }
            // Cancelling/finalizing before exact-episode rebind both destroys
            // the reserved live authority and makes the rebind reject its own
            // now-missing pin.  Keep this legacy destructive repair manual;
            // bounded automatic recovery only performs the pinned adoption.
            if episode.is_none()
                && let Some(owner_channel_id) = relay_frontier_dead_reattach_owner(decision)
            {
                match relay_recovery_probe_snapshot_for_owner(
                    shared.as_ref(),
                    provider,
                    owner_channel_id,
                    decision,
                ) {
                    Ok(probe) => {
                        let expected_watcher =
                            shared.tmux_watchers.get(&owner_channel_id).map(|watcher| {
                                (
                                    watcher.tmux_session_name.clone(),
                                    watcher.output_path.clone(),
                                    watcher.cancel.clone(),
                                )
                            });
                        let gate = super::destructive_cancel_gate::evaluate(
                            shared,
                            provider,
                            owner_channel_id,
                            owner_channel_id,
                            &probe,
                        )
                        .await;
                        if gate.is_allowed() {
                            let current = super::inflight::load_inflight_state(
                                provider,
                                owner_channel_id.get(),
                            );
                            let mailbox_active_user_msg_id =
                                mailbox_snapshot(shared, owner_channel_id)
                                    .await
                                    .active_user_message_id
                                    .map(|id| id.get());
                            let current_matches_probe = current.as_ref().is_some_and(|state| {
                                probe.pin.matches_state(state)
                                    && mailbox_active_user_msg_id
                                        == probe.pin.mailbox_active_user_msg_id
                                    && state.updated_at == probe.updated_at
                                    && state.save_generation == probe.save_generation
                            });
                            if !current_matches_probe {
                                tracing::warn!(
                                    target: "agentdesk::discord::relay_recovery",
                                    provider = provider.as_str(),
                                    channel_id = decision.channel_id,
                                    watcher_owner_channel_id = owner_channel_id.get(),
                                    death_evidence = gate.allowed_reason().unwrap_or("unknown"),
                                    expected_updated_at = %probe.updated_at,
                                    current_updated_at = %current.as_ref().map(|state| state.updated_at.as_str()).unwrap_or("<missing>"),
                                    expected_save_generation = probe.save_generation,
                                    current_save_generation = current.as_ref().map(|state| state.save_generation).unwrap_or(0),
                                    expected_mailbox_active_user_msg_id = probe.pin.mailbox_active_user_msg_id.unwrap_or(0),
                                    mailbox_active_user_msg_id = mailbox_active_user_msg_id.unwrap_or(0),
                                    "relay recovery skipped destructive watcher cancel after gate; owner row changed during death-evidence reprobe"
                                );
                            } else if let Some((tmux_session_name, output_path, cancel)) =
                                expected_watcher.as_ref()
                            {
                                let watcher_removed =
                                    shared.tmux_watchers.cancel_and_remove_channel_if_current(
                                        &owner_channel_id,
                                        tmux_session_name,
                                        output_path,
                                        cancel,
                                    );
                                if !watcher_removed {
                                    tracing::warn!(
                                        target: "agentdesk::discord::relay_recovery",
                                        provider = provider.as_str(),
                                        channel_id = decision.channel_id,
                                        watcher_owner_channel_id = owner_channel_id.get(),
                                        death_evidence = gate.allowed_reason().unwrap_or("unknown"),
                                        "relay recovery skipped destructive watcher cancel after gate; expected watcher was not current"
                                    );
                                } else {
                                    let current =
                                        current.expect("checked by current_matches_probe");
                                    let lifecycle_identity =
                                        super::inflight::InflightTurnIdentity::from_state(&current);
                                    let lifecycle_updated_at = current.updated_at.clone();
                                    let lifecycle_save_generation = current.save_generation;
                                    let finalize_outcome = finalize_cancelled_watcher_owner_turn(
                                        shared,
                                        provider,
                                        decision,
                                        owner_channel_id,
                                    )
                                    .await;
                                    let lifecycle_clear_outcome =
                                        super::inflight::clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence(
                                            provider,
                                            owner_channel_id.get(),
                                            &lifecycle_identity,
                                            &lifecycle_updated_at,
                                            lifecycle_save_generation,
                                        );
                                    tracing::warn!(
                                        target: "agentdesk::discord::relay_recovery",
                                        provider = provider.as_str(),
                                        channel_id = decision.channel_id,
                                        watcher_owner_channel_id = owner_channel_id.get(),
                                        last_relay_offset = decision.evidence.last_relay_offset,
                                        last_capture_offset = ?decision.evidence.last_capture_offset,
                                        unread_bytes = ?decision.evidence.unread_bytes,
                                        death_evidence = gate.allowed_reason().unwrap_or("unknown"),
                                        watcher_removed,
                                        lifecycle_clear_outcome = ?lifecycle_clear_outcome,
                                        finalizer_outcome = match finalize_outcome {
                                            Some(super::turn_finalizer::FinalizeOutcome::Finalized { .. }) => "finalized",
                                            Some(super::turn_finalizer::FinalizeOutcome::AlreadyFinalized) => "already_finalized",
                                            Some(super::turn_finalizer::FinalizeOutcome::Deferred) => "deferred",
                                            None => "missing_identity",
                                        },
                                        "relay recovery cancelled watcher with death evidence before reattach"
                                    );
                                }
                            } else {
                                tracing::warn!(
                                    target: "agentdesk::discord::relay_recovery",
                                    provider = provider.as_str(),
                                    channel_id = decision.channel_id,
                                    watcher_owner_channel_id = owner_channel_id.get(),
                                    death_evidence = gate.allowed_reason().unwrap_or("unknown"),
                                    "relay recovery skipped destructive watcher cancel after gate; no expected watcher identity was captured"
                                );
                            }
                        } else {
                            tracing::warn!(
                                target: "agentdesk::discord::relay_recovery",
                                provider = provider.as_str(),
                                channel_id = decision.channel_id,
                                watcher_owner_channel_id = owner_channel_id.get(),
                                denied_reason = gate.denied_reason().unwrap_or("unknown"),
                                finalizer_turn_id = decision.affected.finalizer_turn_id.unwrap_or(0),
                                mailbox_active_user_msg_id = decision.affected.mailbox_active_user_msg_id.unwrap_or(0),
                                tmux_session = ?decision.affected.tmux_session,
                                "relay recovery skipped destructive watcher cancel; death/identity gate did not pass"
                            );
                        }
                    }
                    Err(reason) => {
                        tracing::warn!(
                            target: "agentdesk::discord::relay_recovery",
                            provider = provider.as_str(),
                            channel_id = decision.channel_id,
                            watcher_owner_channel_id = owner_channel_id.get(),
                            denied_reason = reason,
                            finalizer_turn_id = decision.affected.finalizer_turn_id.unwrap_or(0),
                            mailbox_active_user_msg_id = decision.affected.mailbox_active_user_msg_id.unwrap_or(0),
                            tmux_session = ?decision.affected.tmux_session,
                            "relay recovery skipped destructive watcher cancel; decision identity no longer matches owner row"
                        );
                    }
                }
            }
            reattach_apply::apply_rebind(registry, provider, decision, episode).await
        }
        RelayRecoveryActionKind::DrainPendingQueue => {
            let channel = ChannelId::new(decision.channel_id);
            let outcome = super::health::schedule_pending_queue_drain_after_cancel(
                registry,
                provider.as_str(),
                channel,
                "relay_recovery_queue_blocked",
            )
            .await;
            let after = mailbox_snapshot(shared, channel).await;
            RelayRecoveryApplyResult {
                status: if outcome.queue_depth_after > 0 {
                    "scheduled_pending_queue_drain"
                } else {
                    "pending_queue_empty"
                },
                removed_thread_proofs: 0,
                removed_mailbox_token: false,
                post_mailbox_has_cancel_token: Some(after.cancel_token.is_some()),
                post_mailbox_queue_depth: Some(after.intervention_queue.len()),
                reattach_watcher_spawned: None,
                reattach_watcher_replaced: None,
                reattach_initial_offset: None,
                reattach_error: None,
            }
        }
        RelayRecoveryActionKind::ObserveOnly => RelayRecoveryApplyResult {
            status: "skipped",
            removed_thread_proofs: 0,
            removed_mailbox_token: false,
            post_mailbox_has_cancel_token: None,
            post_mailbox_queue_depth: None,
            reattach_watcher_spawned: None,
            reattach_watcher_replaced: None,
            reattach_initial_offset: None,
            reattach_error: None,
        },
    }
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
mod tests {
    use super::super::relay_health::RelayStallClassifier;
    use super::*;
    use crate::services::provider::{CancelToken, ProviderKind};
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    #[path = "circuit_breaker_apply.rs"]
    mod circuit_breaker_apply;

    fn isolated_agentdesk_root() -> (AgentdeskRootGuard, tempfile::TempDir) {
        let temp = tempfile::TempDir::new().unwrap();
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let guard = AgentdeskRootGuard {
            previous: std::env::var_os("AGENTDESK_ROOT_DIR"),
            _lock: lock,
        };
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        (guard, temp)
    }

    async fn registry_with_shared(provider: ProviderKind) -> (HealthRegistry, Arc<SharedData>) {
        let registry = HealthRegistry::new();
        let shared = super::super::make_shared_data_for_tests();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        (registry, shared)
    }

    async fn start_test_turn(
        shared: &Arc<SharedData>,
        channel: ChannelId,
        message: MessageId,
    ) -> Arc<CancelToken> {
        let token = Arc::new(CancelToken::new());
        let started = super::super::mailbox_try_start_turn(
            shared,
            channel,
            token.clone(),
            UserId::new(1),
            message,
        )
        .await;
        assert!(started, "test mailbox turn should start on an idle channel");
        token
    }

    fn test_watcher_handle(
        tmux_session_name: &str,
        output_path: &std::path::Path,
    ) -> (
        super::super::TmuxWatcherHandle,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        (
            super::super::TmuxWatcherHandle {
                tmux_session_name: tmux_session_name.to_string(),
                output_path: output_path.to_string_lossy().to_string(),
                paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: cancel.clone(),
                pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                    super::super::tmux_watcher_now_ms(),
                )),
            },
            cancel,
        )
    }

    fn snapshot() -> RelayHealthSnapshot {
        RelayHealthSnapshot {
            provider: "codex".to_string(),
            channel_id: 42,
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
            queue_depth: 0,
            pending_discord_callback_msg_id: None,
            pending_thread_proof: false,
            parent_channel_id: None,
            thread_channel_id: None,
            last_relay_ts_ms: None,
            last_outbound_activity_ms: None,
            last_capture_offset: None,
            last_relay_offset: 0,
            unread_bytes: None,
            desynced: false,
            stale_thread_proof: false,
        }
    }

    #[test]
    fn relay_recovery_takeover_forgets_registered_completion_footer_target() {
        let channel_id = ChannelId::new(3_089_203);
        let shared = super::super::make_shared_data_for_tests();
        super::super::footer_view_reconciler::completion_footer_forget_registered_target(
            channel_id,
        );
        let _ = super::super::footer_view_reconciler::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_303),
            &ProviderKind::Codex,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );

        assert!(completion_footer::forget_if_message(
            channel_id,
            Some(3_089_303),
        ));

        assert_eq!(
            super::super::footer_view_reconciler::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                1_800_000_005,
            ),
            None
        );
    }

    #[test]
    fn relay_recovery_takeover_keeps_different_completion_footer_target() {
        let channel_id = ChannelId::new(3_089_213);
        let shared = super::super::make_shared_data_for_tests();
        super::super::footer_view_reconciler::completion_footer_forget_registered_target(
            channel_id,
        );
        let _ = super::super::footer_view_reconciler::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_313),
            &ProviderKind::Codex,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );

        assert!(!completion_footer::forget_if_message(
            channel_id,
            Some(3_089_314),
        ));

        assert!(
            super::super::footer_view_reconciler::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                1_800_000_005,
            )
            .is_some()
        );
        super::super::footer_view_reconciler::completion_footer_forget_registered_target(
            channel_id,
        );
    }

    #[test]
    fn dry_run_plans_safe_stale_thread_proof_cleanup() {
        let decision = plan_relay_recovery(
            &RelayHealthSnapshot {
                pending_thread_proof: true,
                stale_thread_proof: true,
                thread_channel_id: Some(99),
                ..snapshot()
            },
            RelayStallState::StaleThreadProof,
            1_000,
        );

        assert_eq!(
            decision.action,
            RelayRecoveryActionKind::ClearStaleThreadProof
        );
        assert!(decision.auto_heal.eligible);
        assert_eq!(decision.affected.thread_channel_id, Some(99));
        assert_eq!(
            decision.auto_heal.remaining_attempts,
            AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW
        );
    }

    #[test]
    fn active_foreground_stream_is_observe_only() {
        let decision = plan_relay_recovery(
            &RelayHealthSnapshot {
                active_turn: RelayActiveTurn::Foreground,
                mailbox_has_cancel_token: true,
                bridge_inflight_present: true,
                ..snapshot()
            },
            RelayStallState::ActiveForegroundStream,
            1_000,
        );

        assert_eq!(decision.action, RelayRecoveryActionKind::ObserveOnly);
        assert!(!decision.auto_heal.eligible);
        assert_eq!(
            decision.auto_heal.skipped_reason,
            Some("live_foreground_turn")
        );
    }

    #[test]
    fn queue_blocked_schedules_bounded_pending_queue_drain_when_idle() {
        let decision = plan_relay_recovery(
            &RelayHealthSnapshot {
                queue_depth: 2,
                ..snapshot()
            },
            RelayStallState::QueueBlocked,
            1_000,
        );

        assert_eq!(decision.action, RelayRecoveryActionKind::DrainPendingQueue);
        assert_eq!(
            decision.reason,
            "queued work is stranded behind an idle mailbox; bounded queue drain can restore delivery"
        );
        assert!(decision.auto_heal.eligible);
        assert_eq!(decision.auto_heal.skipped_reason, None);
    }

    #[test]
    fn queue_blocked_allows_disk_backed_queue_to_reach_drain_helper() {
        let decision = plan_relay_recovery(
            &RelayHealthSnapshot {
                queue_depth: 0,
                ..snapshot()
            },
            RelayStallState::QueueBlocked,
            1_000,
        );

        assert_eq!(decision.action, RelayRecoveryActionKind::DrainPendingQueue);
        assert!(
            decision.auto_heal.eligible,
            "disk-backed pending queues are hydrated by the drain helper"
        );
        assert_eq!(decision.auto_heal.skipped_reason, None);
    }

    #[test]
    fn queue_blocked_does_not_drain_when_live_turn_evidence_remains() {
        let decision = plan_relay_recovery(
            &RelayHealthSnapshot {
                active_turn: RelayActiveTurn::Foreground,
                mailbox_has_cancel_token: true,
                queue_depth: 2,
                ..snapshot()
            },
            RelayStallState::QueueBlocked,
            1_000,
        );

        assert_eq!(decision.action, RelayRecoveryActionKind::DrainPendingQueue);
        assert!(!decision.auto_heal.eligible);
        assert_eq!(
            decision.auto_heal.skipped_reason,
            Some("queue_blocked_has_live_turn_evidence")
        );
    }

    #[test]
    fn live_agentdesk_tmux_relay_dead_can_reattach_watcher_when_evidence_is_complete() {
        let decision = plan_relay_recovery(
            &RelayHealthSnapshot {
                tmux_session: Some("AgentDesk-codex-42".to_string()),
                tmux_alive: Some(true),
                desynced: true,
                bridge_inflight_present: true,
                mailbox_has_cancel_token: true,
                ..snapshot()
            },
            RelayStallState::TmuxAliveRelayDead,
            1_000,
        );

        assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);
        assert!(decision.auto_heal.eligible);
        assert_eq!(decision.auto_heal.skipped_reason, None);
    }

    #[test]
    fn watcher_owned_live_relay_with_unread_bytes_and_zero_relay_offset_is_actionable() {
        let snapshot = RelayHealthSnapshot {
            provider: "claude".to_string(),
            channel_id: 1509350393350459434,
            active_turn: RelayActiveTurn::Foreground,
            tmux_session: Some("AgentDesk-claude-adk-claude-pipe-e2e".to_string()),
            tmux_alive: Some(true),
            watcher_attached: true,
            watcher_owner_channel_id: Some(1509350393350459434),
            watcher_owns_live_relay: true,
            bridge_inflight_present: true,
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(9001),
            bridge_current_msg_id: Some(9002),
            last_capture_offset: Some(7968),
            last_relay_offset: 0,
            unread_bytes: Some(7968),
            desynced: true,
            ..snapshot()
        };
        let relay_stall_state = RelayStallClassifier::classify(&snapshot);
        let decision = plan_relay_recovery(&snapshot, relay_stall_state, 1_000);

        assert_eq!(relay_stall_state, RelayStallState::TmuxAliveRelayDead);
        assert_ne!(decision.action, RelayRecoveryActionKind::ObserveOnly);
        assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);
        assert!(
            decision.auto_heal.eligible,
            "zero-frontier unread relay-dead turns must be eligible for bounded reattach"
        );
        assert_eq!(decision.auto_heal.skipped_reason, None);
        assert!(
            decision.auto_heal.bounded,
            "relay-dead foreground turns must surface bounded recovery metadata"
        );
        assert_eq!(decision.provider, "claude");
        assert_eq!(decision.channel_id, 1509350393350459434);
        assert_eq!(
            decision.affected.tmux_session.as_deref(),
            Some("AgentDesk-claude-adk-claude-pipe-e2e")
        );
        assert_eq!(decision.evidence.unread_bytes, Some(7968));
        assert_eq!(decision.evidence.last_capture_offset, Some(7968));
        assert_eq!(decision.evidence.last_relay_offset, 0);
        assert_eq!(
            decision.evidence.watcher_owner_channel_id,
            Some(1509350393350459434)
        );
        assert!(decision.evidence.watcher_owns_live_relay);
        assert_eq!(decision.evidence.active_turn, RelayActiveTurn::Foreground);
        assert_eq!(
            relay_frontier_dead_reattach_owner(&decision),
            Some(ChannelId::new(1509350393350459434))
        );
    }

    #[test]
    fn watcher_owned_live_relay_with_relay_progress_is_not_destructive_cancel_candidate() {
        let snapshot = RelayHealthSnapshot {
            provider: "claude".to_string(),
            channel_id: 1509350393350459434,
            active_turn: RelayActiveTurn::Foreground,
            tmux_session: Some("AgentDesk-claude-adk-claude-pipe-e2e".to_string()),
            tmux_alive: Some(true),
            watcher_attached: true,
            watcher_owner_channel_id: Some(1509350393350459434),
            watcher_owns_live_relay: true,
            bridge_inflight_present: true,
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(9001),
            bridge_current_msg_id: Some(9002),
            last_relay_ts_ms: Some(1_777_001_234_000),
            last_capture_offset: Some(7968),
            last_relay_offset: 4096,
            unread_bytes: Some(3872),
            desynced: true,
            ..snapshot()
        };
        let relay_stall_state = RelayStallClassifier::classify(&snapshot);
        let decision = plan_relay_recovery(&snapshot, relay_stall_state, 1_000);

        assert_eq!(relay_stall_state, RelayStallState::ActiveForegroundStream);
        assert_eq!(decision.action, RelayRecoveryActionKind::ObserveOnly);
        assert_eq!(relay_frontier_dead_reattach_owner(&decision), None);

        let forced_dead_decision =
            plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
        assert_eq!(
            relay_frontier_dead_reattach_owner(&forced_dead_decision),
            None,
            "a nonzero relay frontier is progress evidence; even if a later snapshot is \
             relay-dead, recovery must use non-destructive rebind instead of destructive cancel"
        );
    }

    /// #3277 (Defect D) + deploy-preserved ownerless restore eligibility table:
    /// a DEAD attached watcher handle (`watcher_attached_stale`) no longer
    /// blocks the bounded reattach; a genuinely-live watcher that already owns
    /// the relay still does; and a live-but-ownerless watcher is eligible
    /// because rebind only needs to restamp ownership and reuse the incumbent.
    #[test]
    fn reattach_eligibility_distinguishes_stale_attached_watcher_from_live() {
        let base = || RelayHealthSnapshot {
            tmux_session: Some("AgentDesk-claude-adk-cc".to_string()),
            tmux_alive: Some(true),
            desynced: true,
            bridge_inflight_present: true,
            mailbox_has_cancel_token: true,
            ..snapshot()
        };

        // attached + stale handle → dead-handle evidence → eligible.
        let stale_attached = plan_relay_recovery(
            &RelayHealthSnapshot {
                watcher_attached: true,
                watcher_attached_stale: true,
                ..base()
            },
            RelayStallState::TmuxAliveRelayDead,
            1_000,
        );
        assert_eq!(
            stale_attached.action,
            RelayRecoveryActionKind::ReattachWatcher
        );
        assert!(
            stale_attached.auto_heal.eligible,
            "a cancelled/heartbeat-stale attached watcher must not block reattach"
        );
        assert_eq!(stale_attached.auto_heal.skipped_reason, None);

        // attached + LIVE ownerless handle → eligible; this is the
        // post-deploy `watcher_attached=true` / `watcher_owns_live_relay=false`
        // gap where the handle exists but cannot relay the current inflight.
        let live_ownerless_attached = plan_relay_recovery(
            &RelayHealthSnapshot {
                watcher_attached: true,
                watcher_attached_stale: false,
                ..base()
            },
            RelayStallState::TmuxAliveRelayDead,
            1_000,
        );
        assert!(
            live_ownerless_attached.auto_heal.eligible,
            "a live but ownerless watcher should be reused and restamped by reattach"
        );
        assert_eq!(live_ownerless_attached.auto_heal.skipped_reason, None);

        // attached + LIVE owner → never auto-replace a live relay owner.
        let live_owned_attached = plan_relay_recovery(
            &RelayHealthSnapshot {
                watcher_attached: true,
                watcher_attached_stale: false,
                watcher_owns_live_relay: true,
                ..base()
            },
            RelayStallState::TmuxAliveRelayDead,
            1_000,
        );
        assert!(
            !live_owned_attached.auto_heal.eligible,
            "a fresh-heartbeat live watcher that owns relay must keep reattach operator-gated"
        );
        assert_eq!(
            live_owned_attached.auto_heal.skipped_reason,
            Some("reattach_missing_required_live_evidence")
        );

        // detached (legacy case) → still eligible, unchanged.
        let detached = plan_relay_recovery(&base(), RelayStallState::TmuxAliveRelayDead, 1_000);
        assert!(detached.auto_heal.eligible);
    }

    /// #3277 verify-2: the reattach apply reports HONESTLY whether a watcher
    /// was actually spawned (dead incumbent replaced / fresh claim) or a live
    /// same-session incumbent was reused untouched — the latter must not be
    /// labelled "reattached_watcher".
    #[test]
    fn reattach_status_reports_live_incumbent_reuse_honestly() {
        assert_eq!(reattach_apply_status(true), "reattached_watcher");
        assert_eq!(reattach_apply_status(false), "reuse_existing_live_watcher");
    }

    #[test]
    fn live_agentdesk_tmux_relay_dead_without_mailbox_token_can_adopt_ownerless_inflight() {
        let decision = plan_relay_recovery(
            &RelayHealthSnapshot {
                tmux_session: Some("AgentDesk-codex-42".to_string()),
                tmux_alive: Some(true),
                desynced: true,
                bridge_inflight_present: true,
                mailbox_has_cancel_token: false,
                ..snapshot()
            },
            RelayStallState::TmuxAliveRelayDead,
            1_000,
        );

        assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);
        assert!(decision.auto_heal.eligible);
        assert_eq!(decision.auto_heal.skipped_reason, None);
    }

    #[tokio::test]
    async fn dead_frontier_watcher_cancel_finalizes_owner_and_releases_inflight() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let (_root_guard, root_dir) = isolated_agentdesk_root();
        let provider = ProviderKind::Codex;
        let (registry, shared) = registry_with_shared(provider.clone()).await;
        let channel = ChannelId::new(4_030_001);
        let user_msg = MessageId::new(4_030_101);
        let tmux = "AgentDesk-codex-4030-dead-frontier";
        let output_path = root_dir.path().join("watcher-output.jsonl");
        std::fs::write(&output_path, r#"{"type":"thread.started","thread_id":"t"}"#)
            .expect("write output fixture");
        let output_len = std::fs::metadata(&output_path)
            .expect("output fixture metadata")
            .len();
        let token = start_test_turn(&shared, channel, user_msg).await;
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let mut state = super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            user_msg.get(),
            4_030_201,
            "watcher-owned turn".to_string(),
            None,
            Some(tmux.to_string()),
            Some(output_path.to_string_lossy().to_string()),
            None,
            output_len,
        );
        state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui);
        state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
        super::super::inflight::save_inflight_state(&state).expect("save watcher inflight");
        shared.turn_finalizer.register_start(
            super::super::turn_finalizer::TurnKey::new(
                channel,
                state.effective_finalizer_turn_id(),
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::inflight::RelayOwnerKind::Watcher,
            &shared,
        );
        let (watcher, watcher_cancel) = test_watcher_handle(tmux, &output_path);
        watcher.last_heartbeat_ts_ms.store(1, Ordering::Release);
        shared.tmux_watchers.insert(channel, watcher);

        let snapshot = RelayHealthSnapshot {
            provider: provider.as_str().to_string(),
            channel_id: channel.get(),
            active_turn: RelayActiveTurn::Foreground,
            tmux_session: Some(tmux.to_string()),
            tmux_alive: Some(true),
            watcher_attached: true,
            watcher_owner_channel_id: Some(channel.get()),
            watcher_owns_live_relay: true,
            bridge_inflight_present: true,
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(user_msg.get()),
            last_capture_offset: Some(128),
            last_relay_offset: 0,
            unread_bytes: Some(128),
            desynced: true,
            ..snapshot()
        };
        let mut decision =
            plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
        decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());

        let _ = apply_relay_recovery_decision(
            &registry,
            &shared,
            &provider,
            &decision,
            None,
            RelayRecoveryApplySource::ProbeAutoHeal,
        )
        .await;

        assert!(
            watcher_cancel.load(Ordering::Relaxed),
            "relay recovery must still cancel the dead-frontier watcher"
        );
        assert!(
            token.cancelled.load(Ordering::Relaxed),
            "watcher cancel must release the owning mailbox token through the finalizer"
        );
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        assert!(
            super::super::mailbox_snapshot(&shared, channel)
                .await
                .cancel_token
                .is_none(),
            "finalizer-routed watcher cancel must clear active mailbox ownership"
        );
        assert!(
            super::super::inflight::load_inflight_state(&provider, channel.get()).is_none(),
            "finalizer-routed watcher cancel must clear the owning inflight row"
        );
    }

    #[tokio::test]
    async fn reattach_idle_tmux_clear_release_publishes_completion_event() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let (_root_guard, root_dir) = isolated_agentdesk_root();
        let provider = ProviderKind::Claude;
        let (registry, shared) = registry_with_shared(provider.clone()).await;
        let channel = ChannelId::new(4_048_410);
        let user_msg = MessageId::new(4_048_411);
        let tmux = "AgentDesk-claude-4048-reattach-idle-clear";
        let output_path = root_dir.path().join("idle-clear-ready.jsonl");
        let body = "{\"type\":\"system\",\"subtype\":\"turn_duration\",\"session_id\":\"s\"}\n";
        std::fs::write(&output_path, body).expect("write ready output fixture");
        let output_len = std::fs::metadata(&output_path)
            .expect("output fixture metadata")
            .len();
        let token = start_test_turn(&shared, channel, user_msg).await;
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let mut state = super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            user_msg.get(),
            4_048_412,
            "idle tmux cleanup".to_string(),
            None,
            Some(tmux.to_string()),
            Some(output_path.to_string_lossy().to_string()),
            None,
            output_len,
        );
        state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
        super::super::inflight::save_inflight_state(&state).expect("save idle-clear inflight");

        let snapshot = RelayHealthSnapshot {
            provider: provider.as_str().to_string(),
            channel_id: channel.get(),
            active_turn: RelayActiveTurn::Foreground,
            tmux_session: Some(tmux.to_string()),
            tmux_alive: Some(true),
            bridge_inflight_present: true,
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(user_msg.get()),
            last_capture_offset: Some(output_len),
            last_relay_offset: output_len,
            unread_bytes: Some(0),
            desynced: true,
            ..snapshot()
        };
        let decision = plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
        assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);
        assert!(idle_tmux_repair_ready_for_input(
            &provider,
            channel.get(),
            tmux
        ));
        assert!(
            !idle_tmux_repair_has_unrelayed_tail_answer(&state),
            "consumed-at-EOF terminal JSONL must not block idle-tmux cleanup"
        );

        let mut rx =
            super::super::turn_completion_events::subscribe_turn_completion_events(shared.as_ref());
        let result = apply_relay_recovery_decision(
            &registry,
            &shared,
            &provider,
            &decision,
            None,
            RelayRecoveryApplySource::ProbeAutoHeal,
        )
        .await;

        assert_eq!(result.status, "cleared_idle_tmux_stale_turn");
        assert!(result.removed_mailbox_token);
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        let event = rx
            .try_recv()
            .expect("reattach idle-clear mailbox release must publish completion event");
        assert_eq!(event.channel_id, channel);
        assert_eq!(
            shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
            0,
            "release primitive publishes only; the queue listener owns drain/backstop policy"
        );
        assert!(
            super::super::inflight::load_inflight_state(&provider, channel.get()).is_none(),
            "idle-tmux cleanup must clear stale inflight after publishing the release edge"
        );
    }

    #[test]
    fn reattach_idle_tmux_clear_generation_guard_preserves_concurrent_inflight_update() {
        let _guard = auto_heal_test_lock().blocking_lock();
        clear_auto_heal_attempts_for_tests();
        let (_root_guard, root_dir) = isolated_agentdesk_root();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(4_111_003);
        let user_msg = MessageId::new(4_111_103);
        let tmux = "AgentDesk-claude-4111-idle-clear-generation";
        let output_path = root_dir.path().join("idle-clear-generation.jsonl");
        let body = "{\"type\":\"system\",\"subtype\":\"turn_duration\",\"session_id\":\"s\"}\n";
        std::fs::write(&output_path, body).expect("write ready output fixture");

        let mut state = super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            user_msg.get(),
            4_111_203,
            "idle tmux guarded cleanup".to_string(),
            Some("session-4111-idle-clear".to_string()),
            Some(tmux.to_string()),
            Some(output_path.to_string_lossy().to_string()),
            None,
            body.len() as u64,
        );
        state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
        super::super::inflight::save_inflight_state(&state).expect("seed stale idle-clear row");

        let pin = capture_idle_tmux_reattach_inflight_clear_pin(&state)
            .expect("capture clear pin before concurrent writer");
        let mut concurrent = super::super::inflight::load_inflight_state(&provider, channel.get())
            .expect("seeded row for concurrent update");
        concurrent.last_watcher_relayed_offset = Some(8_192);
        concurrent.last_watcher_relayed_generation_mtime_ns = Some(77_777);
        concurrent.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::SessionBoundRelay);
        super::super::inflight::save_inflight_state(&concurrent)
            .expect("save concurrent generation-advancing update");

        assert_eq!(
            clear_idle_tmux_reattach_inflight_if_pinned(&provider, channel.get(), Some(&pin)),
            super::super::inflight::GuardedClearOutcome::UserMsgMismatch,
            "auto reattach idle clear must fail closed when the row save_generation advanced"
        );
        let persisted = super::super::inflight::load_inflight_state(&provider, channel.get())
            .expect("advanced row must survive stale generation clear");
        assert_eq!(persisted.last_watcher_relayed_offset, Some(8_192));
        assert_eq!(
            persisted.last_watcher_relayed_generation_mtime_ns,
            Some(77_777)
        );
        assert_eq!(
            persisted.effective_relay_owner_kind(),
            super::super::inflight::RelayOwnerKind::SessionBoundRelay
        );
    }

    #[tokio::test]
    async fn reattach_idle_tmux_clear_refuses_newer_idle_row_between_predicate_and_pin() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let (_root_guard, root_dir) = isolated_agentdesk_root();
        let provider = ProviderKind::Claude;
        let (registry, shared) = registry_with_shared(provider.clone()).await;
        let channel = ChannelId::new(4_111_004);
        let channel_id = channel.get();
        let stale_user_msg_id = 4_111_104;
        let newer_user_msg_id = 4_111_204;
        let user_msg = MessageId::new(stale_user_msg_id);
        let tmux = "AgentDesk-claude-4111-idle-clear-predicate-pin";
        let output_path = root_dir.path().join("idle-clear-predicate-pin.jsonl");
        let body = "{\"type\":\"system\",\"subtype\":\"turn_duration\",\"session_id\":\"s\"}\n";
        std::fs::write(&output_path, body).expect("write ready output fixture");
        let output_len = std::fs::metadata(&output_path)
            .expect("output fixture metadata")
            .len();
        let token = start_test_turn(&shared, channel, user_msg).await;
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let mut stale = super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            None,
            1,
            stale_user_msg_id,
            4_111_304,
            "stale idle tmux cleanup".to_string(),
            Some("session-4111-idle-clear-stale".to_string()),
            Some(tmux.to_string()),
            Some(output_path.to_string_lossy().to_string()),
            None,
            output_len,
        );
        stale.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
        super::super::inflight::save_inflight_state(&stale).expect("seed stale idle-clear row");

        let hook_provider = provider.clone();
        let hook_tmux = tmux.to_string();
        let hook_output_path = output_path.to_string_lossy().to_string();
        let (watcher, watcher_cancel) = test_watcher_handle(tmux, &output_path);
        shared.tmux_watchers.insert(channel, watcher);
        let footer_msg = MessageId::new(4_111_704);
        super::super::footer_view_reconciler::completion_footer_forget_registered_target(channel);
        let _ = super::super::footer_view_reconciler::register_completion_footer_target(
            channel,
            footer_msg,
            &provider,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );

        let _hook = set_idle_tmux_reattach_inflight_candidate_hook_for_tests(Arc::new(
            move |predicate_snapshot| {
                assert_eq!(
                    predicate_snapshot.user_msg_id, stale_user_msg_id,
                    "hook must receive the stale readiness snapshot before pin capture"
                );
                let mut newer = super::super::inflight::InflightTurnState::new(
                    hook_provider.clone(),
                    channel_id,
                    None,
                    1,
                    newer_user_msg_id,
                    4_111_404,
                    "newer idle tmux cleanup".to_string(),
                    Some("session-4111-idle-clear-newer".to_string()),
                    Some(hook_tmux.clone()),
                    Some(hook_output_path.clone()),
                    None,
                    output_len,
                );
                newer.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
                super::super::inflight::save_inflight_state(&newer)
                    .expect("write newer idle-shaped row before pin capture");
            },
        ));

        let snapshot = RelayHealthSnapshot {
            provider: provider.as_str().to_string(),
            channel_id,
            active_turn: RelayActiveTurn::Foreground,
            tmux_session: Some(tmux.to_string()),
            tmux_alive: Some(true),
            bridge_inflight_present: true,
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(stale_user_msg_id),
            bridge_current_msg_id: Some(footer_msg.get()),
            last_capture_offset: Some(output_len),
            last_relay_offset: output_len,
            unread_bytes: Some(0),
            desynced: true,
            ..snapshot()
        };
        let decision = plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
        assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);

        let result = apply_relay_recovery_decision(
            &registry,
            &shared,
            &provider,
            &decision,
            None,
            RelayRecoveryApplySource::ProbeAutoHeal,
        )
        .await;

        assert_eq!(
            result.status, "skipped_idle_tmux_stale_turn_pin_mismatch",
            "a refused generation-pinned clear must not report the applied clear status"
        );
        assert!(!relay_recovery_status_counts_as_applied(result.status));
        assert_ne!(result.status, "cleared_idle_tmux_stale_turn");
        assert!(!result.removed_mailbox_token);
        assert_eq!(result.post_mailbox_has_cancel_token, Some(true));
        assert!(!token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert!(
            shared.tmux_watchers.contains_key(&channel),
            "skipped clear must preserve the watcher binding for the next watchdog pass"
        );
        assert!(
            !watcher_cancel.load(Ordering::Relaxed),
            "skipped clear must not cancel the watcher binding"
        );
        assert!(
            super::super::mailbox_snapshot(&shared, channel)
                .await
                .cancel_token
                .is_some(),
            "skipped clear must preserve the active mailbox token"
        );
        let persisted = super::super::inflight::load_inflight_state(&provider, channel_id)
            .expect("newer idle-shaped row must survive stale pinned clear");
        assert_eq!(persisted.user_msg_id, newer_user_msg_id);
        assert_eq!(
            persisted.session_id.as_deref(),
            Some("session-4111-idle-clear-newer")
        );
        assert_eq!(
            persisted.effective_relay_owner_kind(),
            super::super::inflight::RelayOwnerKind::Watcher
        );
        assert!(
            super::super::footer_view_reconciler::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel,
                "progress",
                1_800_000_005,
            )
            .is_some(),
            "skipped clear must preserve the registered completion footer target"
        );
        super::super::footer_view_reconciler::completion_footer_forget_registered_target(channel);
    }

    #[tokio::test]
    async fn reattach_idle_tmux_clear_success_tears_down_after_guarded_clear() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let (_root_guard, root_dir) = isolated_agentdesk_root();
        let provider = ProviderKind::Claude;
        let (registry, shared) = registry_with_shared(provider.clone()).await;
        let channel = ChannelId::new(4_111_005);
        let user_msg = MessageId::new(4_111_105);
        let tmux = "AgentDesk-claude-4111-idle-clear-success";
        let output_path = root_dir.path().join("idle-clear-success.jsonl");
        let body = "{\"type\":\"system\",\"subtype\":\"turn_duration\",\"session_id\":\"s\"}\n";
        std::fs::write(&output_path, body).expect("write ready output fixture");
        let output_len = std::fs::metadata(&output_path)
            .expect("output fixture metadata")
            .len();
        let token = start_test_turn(&shared, channel, user_msg).await;
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let mut state = super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            user_msg.get(),
            4_111_305,
            "idle tmux guarded cleanup success".to_string(),
            Some("session-4111-idle-clear-success".to_string()),
            Some(tmux.to_string()),
            Some(output_path.to_string_lossy().to_string()),
            None,
            output_len,
        );
        state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
        super::super::inflight::save_inflight_state(&state).expect("seed idle-clear row");
        let (watcher, watcher_cancel) = test_watcher_handle(tmux, &output_path);
        shared.tmux_watchers.insert(channel, watcher);

        let snapshot = RelayHealthSnapshot {
            provider: provider.as_str().to_string(),
            channel_id: channel.get(),
            active_turn: RelayActiveTurn::Foreground,
            tmux_session: Some(tmux.to_string()),
            tmux_alive: Some(true),
            watcher_attached: true,
            watcher_attached_stale: true,
            watcher_owner_channel_id: Some(channel.get()),
            watcher_owns_live_relay: true,
            bridge_inflight_present: true,
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(user_msg.get()),
            last_capture_offset: Some(output_len),
            last_relay_offset: output_len,
            unread_bytes: Some(0),
            desynced: true,
            ..snapshot()
        };
        let decision = plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
        assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);

        let result = apply_relay_recovery_decision(
            &registry,
            &shared,
            &provider,
            &decision,
            None,
            RelayRecoveryApplySource::ProbeAutoHeal,
        )
        .await;

        assert_eq!(result.status, "cleared_idle_tmux_stale_turn");
        assert!(relay_recovery_status_counts_as_applied(result.status));
        assert!(result.removed_mailbox_token);
        assert_eq!(result.post_mailbox_has_cancel_token, Some(false));
        assert_eq!(result.reattach_watcher_replaced, Some(true));
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert!(watcher_cancel.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        assert!(
            !shared.tmux_watchers.contains_key(&channel),
            "successful guarded clear must remove the retired watcher binding"
        );
        assert!(
            super::super::mailbox_snapshot(&shared, channel)
                .await
                .cancel_token
                .is_none(),
            "successful guarded clear must release the mailbox token"
        );
        assert!(
            super::super::inflight::load_inflight_state(&provider, channel.get()).is_none(),
            "successful guarded clear must remove the pinned inflight row before teardown completes"
        );
    }

    #[tokio::test]
    async fn stale_watcher_with_jsonl_progress_rebinds_without_canceling_turn() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let (_root_guard, root_dir) = isolated_agentdesk_root();
        let provider = ProviderKind::Codex;
        let (registry, shared) = registry_with_shared(provider.clone()).await;
        let channel = ChannelId::new(4_030_004);
        let user_msg = MessageId::new(4_030_104);
        let tmux = "AgentDesk-codex-4030-jsonl-progress";
        let output_path = root_dir.path().join("jsonl-progress.jsonl");
        std::fs::write(&output_path, "chunk-1").expect("write output fixture");
        let token = start_test_turn(&shared, channel, user_msg).await;
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let mut state = super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            user_msg.get(),
            4_030_204,
            "watcher-owned active turn".to_string(),
            None,
            Some(tmux.to_string()),
            Some(output_path.to_string_lossy().to_string()),
            None,
            0,
        );
        state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
        super::super::inflight::save_inflight_state(&state).expect("save watcher inflight");
        shared.turn_finalizer.register_start(
            super::super::turn_finalizer::TurnKey::new(
                channel,
                state.effective_finalizer_turn_id(),
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::inflight::RelayOwnerKind::Watcher,
            &shared,
        );
        let (watcher, _) = test_watcher_handle(tmux, &output_path);
        watcher.last_heartbeat_ts_ms.store(1, Ordering::Release);
        shared.tmux_watchers.insert(channel, watcher);

        let snapshot = RelayHealthSnapshot {
            provider: provider.as_str().to_string(),
            channel_id: channel.get(),
            active_turn: RelayActiveTurn::Foreground,
            tmux_session: Some(tmux.to_string()),
            tmux_alive: Some(true),
            watcher_attached: true,
            watcher_owner_channel_id: Some(channel.get()),
            watcher_owns_live_relay: true,
            bridge_inflight_present: true,
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(user_msg.get()),
            last_capture_offset: Some(128),
            last_relay_offset: 0,
            unread_bytes: Some(128),
            desynced: true,
            ..snapshot()
        };
        let mut decision =
            plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
        decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());

        let output_for_task = output_path.clone();
        let progress = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            std::fs::write(&output_for_task, "chunk-1\nchunk-2").expect("append output fixture");
        });
        let _ = apply_relay_recovery_decision(
            &registry,
            &shared,
            &provider,
            &decision,
            None,
            RelayRecoveryApplySource::ProbeAutoHeal,
        )
        .await;
        progress.await.expect("jsonl progress task");

        assert!(
            !token.cancelled.load(Ordering::Relaxed),
            "watcher-heartbeat stale plus active JSONL progress is not turn-death evidence"
        );
        let current = super::super::inflight::load_inflight_state(&provider, channel.get())
            .expect("active turn inflight must survive watcher rebind");
        assert_eq!(current.user_msg_id, user_msg.get());
    }

    #[tokio::test]
    async fn fresh_watcher_heartbeat_blocks_destructive_cancel_before_reattach() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let (_root_guard, root_dir) = isolated_agentdesk_root();
        let provider = ProviderKind::Codex;
        let (registry, shared) = registry_with_shared(provider.clone()).await;
        let channel = ChannelId::new(4_030_002);
        let user_msg = MessageId::new(4_030_102);
        let tmux = "AgentDesk-codex-4030-fresh-heartbeat";
        let output_path = root_dir.path().join("fresh-heartbeat.jsonl");
        std::fs::write(&output_path, "still growing soon").expect("write output fixture");
        let token = start_test_turn(&shared, channel, user_msg).await;
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let mut state = super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            user_msg.get(),
            4_030_202,
            "watcher-owned live turn".to_string(),
            None,
            Some(tmux.to_string()),
            Some(output_path.to_string_lossy().to_string()),
            None,
            0,
        );
        state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
        super::super::inflight::save_inflight_state(&state).expect("save watcher inflight");
        shared.turn_finalizer.register_start(
            super::super::turn_finalizer::TurnKey::new(
                channel,
                state.effective_finalizer_turn_id(),
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::inflight::RelayOwnerKind::Watcher,
            &shared,
        );
        let (watcher, watcher_cancel) = test_watcher_handle(tmux, &output_path);
        shared.tmux_watchers.insert(channel, watcher);

        let snapshot = RelayHealthSnapshot {
            provider: provider.as_str().to_string(),
            channel_id: channel.get(),
            active_turn: RelayActiveTurn::Foreground,
            tmux_session: Some(tmux.to_string()),
            tmux_alive: Some(true),
            watcher_attached: true,
            watcher_owner_channel_id: Some(channel.get()),
            watcher_owns_live_relay: true,
            bridge_inflight_present: true,
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(user_msg.get()),
            last_capture_offset: Some(128),
            last_relay_offset: 0,
            unread_bytes: Some(128),
            desynced: true,
            ..snapshot()
        };
        let mut decision =
            plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
        decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());

        let _ = apply_relay_recovery_decision(
            &registry,
            &shared,
            &provider,
            &decision,
            None,
            RelayRecoveryApplySource::Manual,
        )
        .await;

        assert!(
            !watcher_cancel.load(Ordering::Relaxed),
            "a fresh heartbeat watcher must never be destructively cancelled"
        );
        assert!(
            !token.cancelled.load(Ordering::Relaxed),
            "fresh-heartbeat gate must preserve the live turn's mailbox token"
        );
        let current = super::super::inflight::load_inflight_state(&provider, channel.get())
            .expect("fresh-heartbeat gate must preserve inflight");
        assert_eq!(current.user_msg_id, user_msg.get());
    }

    #[tokio::test]
    async fn relay_recovery_identity_pin_preserves_t2_started_after_t1_snapshot() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let (_root_guard, root_dir) = isolated_agentdesk_root();
        let provider = ProviderKind::Codex;
        let (registry, shared) = registry_with_shared(provider.clone()).await;
        let channel = ChannelId::new(4_030_003);
        let t1_msg = MessageId::new(4_030_103);
        let t2_msg = MessageId::new(4_030_104);
        let tmux = "AgentDesk-codex-4030-t1-t2";
        let t1_output = root_dir.path().join("t1.jsonl");
        let t2_output = root_dir.path().join("t2.jsonl");
        std::fs::write(&t1_output, "turn one tail").expect("write t1 output fixture");
        std::fs::write(&t2_output, "turn two tail").expect("write t2 output fixture");
        let _t1_token = start_test_turn(&shared, channel, t1_msg).await;
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let mut t1_state = super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            t1_msg.get(),
            4_030_203,
            "turn one".to_string(),
            None,
            Some(tmux.to_string()),
            Some(t1_output.to_string_lossy().to_string()),
            None,
            0,
        );
        t1_state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
        super::super::inflight::save_inflight_state(&t1_state).expect("save t1 inflight");
        shared.turn_finalizer.register_start(
            super::super::turn_finalizer::TurnKey::new(
                channel,
                t1_state.effective_finalizer_turn_id(),
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::inflight::RelayOwnerKind::Watcher,
            &shared,
        );
        let (watcher, _) = test_watcher_handle(tmux, &t1_output);
        watcher.last_heartbeat_ts_ms.store(1, Ordering::Release);
        shared.tmux_watchers.insert(channel, watcher);

        let snapshot = RelayHealthSnapshot {
            provider: provider.as_str().to_string(),
            channel_id: channel.get(),
            active_turn: RelayActiveTurn::Foreground,
            tmux_session: Some(tmux.to_string()),
            tmux_alive: Some(true),
            watcher_attached: true,
            watcher_owner_channel_id: Some(channel.get()),
            watcher_owns_live_relay: true,
            bridge_inflight_present: true,
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(t1_msg.get()),
            last_capture_offset: Some(64),
            last_relay_offset: 0,
            unread_bytes: Some(64),
            desynced: true,
            ..snapshot()
        };
        let mut decision =
            plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
        decision.affected.finalizer_turn_id = Some(t1_state.effective_finalizer_turn_id());

        let _ = mailbox_finish_turn(&shared, &provider, channel).await;
        let t2_token = start_test_turn(&shared, channel, t2_msg).await;
        shared.restart.global_active.store(1, Ordering::Relaxed);
        let mut t2_state = super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            t2_msg.get(),
            4_030_204,
            "turn two".to_string(),
            None,
            Some(tmux.to_string()),
            Some(t2_output.to_string_lossy().to_string()),
            None,
            0,
        );
        t2_state.set_relay_owner_kind(super::super::inflight::RelayOwnerKind::Watcher);
        super::super::inflight::save_inflight_state(&t2_state).expect("save t2 inflight");
        shared.turn_finalizer.register_start(
            super::super::turn_finalizer::TurnKey::new(
                channel,
                t2_state.effective_finalizer_turn_id(),
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::inflight::RelayOwnerKind::Watcher,
            &shared,
        );

        let _ = apply_relay_recovery_decision(
            &registry,
            &shared,
            &provider,
            &decision,
            None,
            RelayRecoveryApplySource::Manual,
        )
        .await;

        assert!(
            !t2_token.cancelled.load(Ordering::Relaxed),
            "T1's pinned recovery decision must no-op instead of canceling T2"
        );
        let current = super::super::inflight::load_inflight_state(&provider, channel.get())
            .expect("T2 inflight must survive the stale T1 recovery apply");
        assert_eq!(current.user_msg_id, t2_msg.get());
        assert_eq!(
            super::super::mailbox_snapshot(&shared, channel)
                .await
                .active_user_message_id
                .map(|id| id.get()),
            Some(t2_msg.get())
        );
    }

    #[test]
    fn orphan_pending_token_is_auto_heal_candidate_only_without_live_evidence() {
        let decision = plan_relay_recovery(
            &RelayHealthSnapshot {
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: Some(9001),
                ..snapshot()
            },
            RelayStallState::OrphanPendingToken,
            1_000,
        );

        assert_eq!(
            decision.action,
            RelayRecoveryActionKind::ClearOrphanPendingToken
        );
        assert!(decision.auto_heal.eligible);

        let live = plan_relay_recovery(
            &RelayHealthSnapshot {
                mailbox_has_cancel_token: true,
                watcher_attached: true,
                ..snapshot()
            },
            RelayStallState::OrphanPendingToken,
            1_000,
        );
        assert!(!live.auto_heal.eligible);
        assert_eq!(
            live.auto_heal.skipped_reason,
            Some("orphan_token_has_live_evidence")
        );
    }

    #[tokio::test]
    async fn auto_heal_attempts_are_rate_limited_per_window() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let key = auto_heal_key(
            "codex",
            42,
            RelayRecoveryActionKind::ClearOrphanPendingToken,
            RelayRecoveryApplySource::ProbeAutoHeal,
        );

        assert_eq!(
            reserve_auto_heal_attempt(&key, 1_000, AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW),
            Ok(0)
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, 2_000, AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW),
            Err("auto_heal_rate_limited")
        );
        assert_eq!(
            reserve_auto_heal_attempt(
                &key,
                1_000 + AUTO_HEAL_WINDOW_SECS * 1000,
                AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW
            ),
            Ok(0)
        );
    }

    #[tokio::test]
    async fn dead_frontier_reattach_gets_one_bounded_retry_only() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let snapshot = RelayHealthSnapshot {
            provider: "codex".to_string(),
            channel_id: 3_779_001,
            active_turn: RelayActiveTurn::Foreground,
            tmux_session: Some("AgentDesk-codex-retry-dead-frontier".to_string()),
            tmux_alive: Some(true),
            watcher_attached: true,
            watcher_owner_channel_id: Some(3_779_001),
            watcher_owns_live_relay: true,
            bridge_inflight_present: true,
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(3_779_101),
            last_capture_offset: Some(2_048),
            last_relay_offset: 0,
            unread_bytes: Some(2_048),
            desynced: true,
            ..snapshot()
        };
        let decision = plan_relay_recovery(&snapshot, RelayStallState::TmuxAliveRelayDead, 1_000);
        let key = auto_heal_key(
            &decision.provider,
            decision.channel_id,
            decision.action,
            RelayRecoveryApplySource::Manual,
        );

        assert_eq!(decision.action, RelayRecoveryActionKind::ReattachWatcher);
        assert!(decision.auto_heal.eligible);
        assert_eq!(
            decision.auto_heal.max_attempts_per_window,
            AUTO_HEAL_DEAD_FRONTIER_REATTACH_MAX_ATTEMPTS_PER_WINDOW
        );
        assert_eq!(decision.auto_heal.remaining_attempts, 2);
        assert_eq!(
            reserve_auto_heal_attempt(&key, 1_000, decision.auto_heal.max_attempts_per_window),
            Ok(1)
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, 2_000, decision.auto_heal.max_attempts_per_window),
            Ok(0),
            "a still-dead relay frontier gets one bounded non-destructive reattach retry"
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, 3_000, decision.auto_heal.max_attempts_per_window),
            Err("auto_heal_rate_limited")
        );

        let progressed = plan_relay_recovery(
            &RelayHealthSnapshot {
                last_relay_ts_ms: Some(2_500),
                last_relay_offset: 512,
                unread_bytes: Some(1_536),
                ..snapshot
            },
            RelayStallState::TmuxAliveRelayDead,
            3_000,
        );
        assert_eq!(
            progressed.auto_heal.max_attempts_per_window, AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW,
            "once the relay frontier advances, reattach returns to the default limiter"
        );
        assert_eq!(progressed.auto_heal.remaining_attempts, 0);
    }

    #[tokio::test]
    async fn auto_apply_orphan_pending_token_clears_mailbox_token() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let (_root_guard, _root_dir) = isolated_agentdesk_root();
        let provider = ProviderKind::Codex;
        let (registry, shared) = registry_with_shared(provider.clone()).await;
        let channel = ChannelId::new(3_360_001);
        let token = start_test_turn(&shared, channel, MessageId::new(91)).await;
        token
            .tmux_session
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .replace("AgentDesk-codex-3360-dead-token-session".to_string());
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let response = auto_apply_relay_recovery_for_shared(
            &registry,
            shared.clone(),
            &provider,
            channel.get(),
            RelayRecoveryActionKind::ClearOrphanPendingToken,
            RelayRecoveryApplySource::ProbeAutoHeal,
        )
        .await
        .expect("orphan token auto-heal should evaluate");

        assert!(response.applied);
        assert_eq!(
            response.decision.action,
            RelayRecoveryActionKind::ClearOrphanPendingToken
        );
        assert_eq!(response.decision.evidence.tmux_alive, Some(false));
        assert!(
            response
                .apply_result
                .as_ref()
                .is_some_and(|result| result.removed_mailbox_token)
        );
        assert!(
            super::super::mailbox_snapshot(&shared, channel)
                .await
                .cancel_token
                .is_none()
        );
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn probe_auto_apply_is_rate_limited_per_channel_action() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let (_root_guard, _root_dir) = isolated_agentdesk_root();
        let provider = ProviderKind::Codex;
        let (registry, shared) = registry_with_shared(provider.clone()).await;
        let channel = ChannelId::new(3_360_002);
        start_test_turn(&shared, channel, MessageId::new(92)).await;

        let first = auto_apply_relay_recovery_for_shared(
            &registry,
            shared.clone(),
            &provider,
            channel.get(),
            RelayRecoveryActionKind::ClearOrphanPendingToken,
            RelayRecoveryApplySource::ProbeAutoHeal,
        )
        .await
        .expect("first orphan token auto-heal should evaluate");
        assert!(first.applied);

        start_test_turn(&shared, channel, MessageId::new(93)).await;
        let second = auto_apply_relay_recovery_for_shared(
            &registry,
            shared.clone(),
            &provider,
            channel.get(),
            RelayRecoveryActionKind::ClearOrphanPendingToken,
            RelayRecoveryApplySource::ProbeAutoHeal,
        )
        .await
        .expect("second orphan token auto-heal should evaluate");

        assert!(second.skipped);
        assert!(!second.applied);
        assert_eq!(
            second.decision.auto_heal.skipped_reason,
            Some("auto_heal_rate_limited")
        );
        assert!(
            super::super::mailbox_snapshot(&shared, channel)
                .await
                .cancel_token
                .is_some(),
            "rate-limited auto-heal must leave the token untouched"
        );
    }

    #[tokio::test]
    async fn watchdog_auto_apply_is_rate_limited_after_first_token_reclaim() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let (_root_guard, _root_dir) = isolated_agentdesk_root();
        let provider = ProviderKind::Codex;
        let (registry, shared) = registry_with_shared(provider.clone()).await;
        let channel = ChannelId::new(3_360_005);

        let first_token = start_test_turn(&shared, channel, MessageId::new(94)).await;
        first_token
            .tmux_session
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .replace("AgentDesk-codex-3360-watchdog-first".to_string());
        shared.restart.global_active.store(1, Ordering::Relaxed);
        let first = auto_apply_relay_recovery_for_shared(
            &registry,
            shared.clone(),
            &provider,
            channel.get(),
            RelayRecoveryActionKind::ClearOrphanPendingToken,
            RelayRecoveryApplySource::StallWatchdog,
        )
        .await
        .expect("first watchdog orphan token auto-heal should evaluate");

        assert!(first.applied);
        assert!(!first.skipped);
        assert_eq!(
            first.decision.action,
            RelayRecoveryActionKind::ClearOrphanPendingToken
        );
        assert_eq!(
            first.decision.auto_heal.skipped_reason, None,
            "the first watchdog reclaim in a fresh window must pass"
        );
        assert!(
            first
                .apply_result
                .as_ref()
                .is_some_and(|result| result.removed_mailbox_token)
        );
        assert!(
            super::super::mailbox_snapshot(&shared, channel)
                .await
                .cancel_token
                .is_none()
        );
        assert!(first_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);

        let second_token = start_test_turn(&shared, channel, MessageId::new(95)).await;
        second_token
            .tmux_session
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .replace("AgentDesk-codex-3360-watchdog-second".to_string());
        shared.restart.global_active.store(1, Ordering::Relaxed);
        let second = auto_apply_relay_recovery_for_shared(
            &registry,
            shared.clone(),
            &provider,
            channel.get(),
            RelayRecoveryActionKind::ClearOrphanPendingToken,
            RelayRecoveryApplySource::StallWatchdog,
        )
        .await
        .expect("second watchdog orphan token auto-heal should evaluate");

        assert!(second.skipped);
        assert!(!second.applied);
        assert_eq!(
            second.decision.auto_heal.skipped_reason,
            Some("auto_heal_rate_limited")
        );
        assert!(
            super::super::mailbox_snapshot(&shared, channel)
                .await
                .cancel_token
                .is_some(),
            "rate-limited watchdog auto-heal must leave the token untouched"
        );
        assert!(!second_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn auto_apply_is_limited_to_requested_action_kind() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
        let (_root_guard, _root_dir) = isolated_agentdesk_root();
        let provider = ProviderKind::Codex;
        let (registry, shared) = registry_with_shared(provider.clone()).await;
        let parent = ChannelId::new(3_360_003);
        let thread = ChannelId::new(3_360_004);
        shared.dispatch.thread_parents.insert(parent, thread);

        let response = auto_apply_relay_recovery_for_shared(
            &registry,
            shared.clone(),
            &provider,
            parent.get(),
            RelayRecoveryActionKind::ClearOrphanPendingToken,
            RelayRecoveryApplySource::ProbeAutoHeal,
        )
        .await
        .expect("stale thread proof decision should evaluate");

        assert!(response.skipped);
        assert_eq!(
            response.decision.action,
            RelayRecoveryActionKind::ClearStaleThreadProof
        );
        assert_eq!(
            response.decision.auto_heal.skipped_reason,
            Some("auto_heal_action_not_allowed")
        );
        assert!(
            shared.dispatch.thread_parents.contains_key(&parent),
            "auto orphan cleanup must not apply other recovery action kinds"
        );
    }

    // #3668 F2: the destructive idle-tmux clear must not drop a final answer that
    // is still persisted in JSONL after `last_offset`. The guard reads the same
    // offset slice via `extract_response_from_output_pub`; when it yields
    // non-empty text the caller skips the destructive clear (rebind fall-
    // through). When the tail is genuinely empty the guard is silent and the
    // existing clear behavior is preserved.
    struct AgentdeskRootGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for AgentdeskRootGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn write_inflight_with_output(
        provider: &ProviderKind,
        channel_id: u64,
        output_path: &std::path::Path,
        last_offset: u64,
        jsonl_body: &str,
    ) {
        std::fs::write(output_path, jsonl_body).expect("write output jsonl");
        let state = super::super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("adk-cdx".to_string()),
            7,
            777,
            7777,
            "hello".to_string(),
            None,
            Some(format!("AgentDesk-codex-adk-cdx-{channel_id}")),
            Some(output_path.to_string_lossy().to_string()),
            None,
            last_offset,
        );
        // full_response stays empty (streaming guard would pass): F2 reproduces
        // the empty-stream + JSONL-terminal-answer asymmetry exactly.
        assert!(state.full_response.is_empty());
        super::super::inflight::save_inflight_state(&state).expect("save inflight");
    }

    fn set_output_mtime_age(output_path: &std::path::Path, age: std::time::Duration) {
        let modified = std::time::SystemTime::now()
            .checked_sub(age)
            .expect("mtime before now");
        filetime::set_file_mtime(output_path, filetime::FileTime::from_system_time(modified))
            .expect("set output mtime");
    }

    #[test]
    fn frozen_busy_jsonl_uses_ready_pane_fallback_after_stale_window() {
        let _guard = auto_heal_test_lock().blocking_lock();
        let (_root_guard, temp) = isolated_agentdesk_root();

        let provider = ProviderKind::Claude;
        let channel_id = 4_030_501;
        let output_path = temp.path().join("frozen-busy-ready.jsonl");
        let body = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"streaming tail without terminator\"}]}}\n";
        write_inflight_with_output(&provider, channel_id, &output_path, body.len() as u64, body);
        set_output_mtime_age(&output_path, std::time::Duration::from_secs(20 * 60));

        assert!(
            idle_tmux_repair_ready_for_input_with_pane_probe(
                &provider,
                channel_id,
                "tmux-4030-frozen-ready",
                |_tmux, _provider| true,
            ),
            "a long-frozen Busy JSONL may consume the pane-ready fallback"
        );
    }

    #[test]
    fn frozen_busy_jsonl_keeps_deny_when_pane_still_busy() {
        let _guard = auto_heal_test_lock().blocking_lock();
        let (_root_guard, temp) = isolated_agentdesk_root();

        let provider = ProviderKind::Claude;
        let channel_id = 4_030_502;
        let output_path = temp.path().join("frozen-busy-pane-busy.jsonl");
        let body = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"live long-running tool call\"}]}}\n";
        write_inflight_with_output(&provider, channel_id, &output_path, body.len() as u64, body);
        set_output_mtime_age(&output_path, std::time::Duration::from_secs(20 * 60));

        assert!(
            !idle_tmux_repair_ready_for_input_with_pane_probe(
                &provider,
                channel_id,
                "tmux-4030-frozen-busy",
                |_tmux, _provider| false,
            ),
            "a frozen Busy JSONL still denies while the live pane is not ready"
        );
    }

    #[test]
    fn idle_tmux_repair_guard_detects_tail_answer_after_offset() {
        let _guard = auto_heal_test_lock().blocking_lock();
        let (_root_guard, temp) = isolated_agentdesk_root();

        let provider = ProviderKind::Codex;
        let channel_id = 3_668_001;
        let output_path = temp.path().join("out.jsonl");

        // A leading pre-offset record (consumed) followed by a terminal answer
        // record after `last_offset`. `last_offset` points past the first line so
        // only the final answer remains in the extracted slice.
        let pre = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"old\"}]}}\n";
        let post = "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"FINAL ANSWER\"}\n";
        let last_offset = pre.len() as u64;
        write_inflight_with_output(
            &provider,
            channel_id,
            &output_path,
            last_offset,
            &format!("{pre}{post}"),
        );
        let state = super::super::inflight::load_inflight_state(&provider, channel_id)
            .expect("tail-answer guard fixture must save an inflight row");

        assert!(
            idle_tmux_repair_has_unrelayed_tail_answer(&state),
            "JSONL terminal answer after last_offset must block destructive clear"
        );
    }

    #[test]
    fn idle_tmux_repair_guard_silent_when_tail_empty() {
        let _guard = auto_heal_test_lock().blocking_lock();
        let (_root_guard, temp) = isolated_agentdesk_root();

        let provider = ProviderKind::Codex;
        let channel_id = 3_668_002;
        let output_path = temp.path().join("out.jsonl");

        // Only a pre-offset record exists; nothing relayable remains after
        // `last_offset`, so the guard stays silent and the existing destructive
        // clear behavior is preserved (behavior-preserving regression guard).
        let body = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"old\"}]}}\n";
        let last_offset = body.len() as u64;
        write_inflight_with_output(&provider, channel_id, &output_path, last_offset, body);
        let state = super::super::inflight::load_inflight_state(&provider, channel_id)
            .expect("empty-tail guard fixture must save an inflight row");

        assert!(
            !idle_tmux_repair_has_unrelayed_tail_answer(&state),
            "empty JSONL tail must not block the existing destructive clear path"
        );
    }

    #[test]
    fn idle_tmux_repair_guard_silent_when_partial_text_has_no_terminal_result() {
        // #3668 codex r3: a hung/desynced turn that emitted partial assistant
        // text after `last_offset` but NO terminal `result` record must NOT
        // suppress the destructive clear — otherwise the watchdog would skip it
        // every tick forever (recovery only advances the offset on terminal
        // success). The guard requires success-result completion evidence.
        let _guard = auto_heal_test_lock().blocking_lock();
        let (_root_guard, temp) = isolated_agentdesk_root();

        let provider = ProviderKind::Codex;
        let channel_id = 3_668_003;
        let output_path = temp.path().join("out.jsonl");

        let pre = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"old\"}]}}\n";
        let post = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"partial, still streaming...\"}]}}\n";
        let last_offset = pre.len() as u64;
        write_inflight_with_output(
            &provider,
            channel_id,
            &output_path,
            last_offset,
            &format!("{pre}{post}"),
        );
        let state = super::super::inflight::load_inflight_state(&provider, channel_id)
            .expect("partial-tail guard fixture must save an inflight row");

        assert!(
            !idle_tmux_repair_has_unrelayed_tail_answer(&state),
            "partial assistant text without a terminal success result must not block force-clean"
        );
    }
}
