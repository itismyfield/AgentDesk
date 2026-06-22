//! Relay recovery dry-run planner and conservative auto-heal executor.
//!
//! This module is intentionally narrow: it turns the read-only relay health
//! classifier into an operator-facing decision, and only applies local,
//! idempotent cleanup when the evidence is strong enough.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

use poise::serenity_prelude::ChannelId;
use serde::Serialize;

use super::health::HealthRegistry;
use super::relay_health::{RelayActiveTurn, RelayHealthSnapshot, RelayStallState};
use super::{
    SharedData, mailbox_clear_channel, mailbox_clear_recovery_marker, mailbox_finish_turn,
    mailbox_snapshot,
};
use crate::services::provider::ProviderKind;

const AUTO_HEAL_WINDOW_SECS: i64 = 600;
const AUTO_HEAL_MAX_ATTEMPTS_PER_WINDOW: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum RelayRecoveryActionKind {
    ObserveOnly,
    ClearStaleThreadProof,
    ClearOrphanPendingToken,
    ReattachWatcher,
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

#[derive(Clone, Copy, Debug)]
struct AttemptWindow {
    window_start_ms: i64,
    attempts: u32,
}

fn auto_heal_attempts() -> &'static Mutex<HashMap<String, AttemptWindow>> {
    static ATTEMPTS: OnceLock<Mutex<HashMap<String, AttemptWindow>>> = OnceLock::new();
    // Short-lived process memory guard only; persistence across restarts is out
    // of scope for this bounded local auto-heal limiter.
    ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn auto_heal_key(provider: &str, channel_id: u64, action: RelayRecoveryActionKind) -> String {
    format!("{}:{}:{}", provider, channel_id, action.as_str())
}

fn remaining_auto_heal_attempts(key: &str, now_ms: i64) -> u32 {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    let Some(window) = attempts.get_mut(key) else {
        return AUTO_HEAL_MAX_ATTEMPTS_PER_WINDOW;
    };
    if now_ms.saturating_sub(window.window_start_ms) >= AUTO_HEAL_WINDOW_SECS * 1000 {
        attempts.remove(key);
        return AUTO_HEAL_MAX_ATTEMPTS_PER_WINDOW;
    }
    AUTO_HEAL_MAX_ATTEMPTS_PER_WINDOW.saturating_sub(window.attempts)
}

fn reserve_auto_heal_attempt(key: &str, now_ms: i64) -> Result<u32, &'static str> {
    let mut attempts = auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned");
    let window = attempts.entry(key.to_string()).or_insert(AttemptWindow {
        window_start_ms: now_ms,
        attempts: 0,
    });
    if now_ms.saturating_sub(window.window_start_ms) >= AUTO_HEAL_WINDOW_SECS * 1000 {
        window.window_start_ms = now_ms;
        window.attempts = 0;
    }
    if window.attempts >= AUTO_HEAL_MAX_ATTEMPTS_PER_WINDOW {
        return Err("auto_heal_rate_limited");
    }
    window.attempts += 1;
    Ok(AUTO_HEAL_MAX_ATTEMPTS_PER_WINDOW.saturating_sub(window.attempts))
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
    // #3277 (Defect D): a watcher binding whose handle is provably DEAD
    // (cancelled or heartbeat-stale — `watcher_attached_stale`) must not
    // block the bounded reattach the way a genuinely-live watcher does. A
    // fresh-heartbeat live watcher still makes this ineligible: auto-heal
    // never replaces a live handle (that case is the finalizer far-backstop's
    // job, #3277 Defect C).
    snapshot.tmux_alive == Some(true)
        && snapshot.bridge_inflight_present
        && snapshot.mailbox_has_cancel_token
        && (!snapshot.watcher_attached
            || snapshot.watcher_attached_stale
            || !snapshot.watcher_owns_live_relay)
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
    let key = auto_heal_key(&snapshot.provider, snapshot.channel_id, action);
    RelayRecoveryAutoHeal {
        eligible,
        bounded: true,
        max_attempts_per_window: AUTO_HEAL_MAX_ATTEMPTS_PER_WINDOW,
        window_secs: AUTO_HEAL_WINDOW_SECS,
        remaining_attempts: remaining_auto_heal_attempts(&key, now_ms),
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
        RelayStallState::QueueBlocked => (
            RelayRecoveryActionKind::ObserveOnly,
            "queued work is already surfaced as degraded health; relay recovery has no safe local cleanup",
            false,
            Some("operator_inspection_required"),
        ),
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
    let decision = plan_relay_recovery(&snapshot.relay_health, snapshot.relay_stall_state, now_ms);
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
    let shared = registry
        .shared_for_provider_on_channel(&provider, ChannelId::new(decision.channel_id))
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

async fn apply_relay_recovery_plan(
    registry: &HealthRegistry,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    mut decision: RelayRecoveryDecision,
    now_ms: i64,
    source: RelayRecoveryApplySource,
) -> RelayRecoveryResponse {
    if !decision.auto_heal.eligible {
        trace_relay_recovery_skipped(&decision, decision.auto_heal.skipped_reason);
        return RelayRecoveryResponse {
            ok: false,
            mode: "apply",
            applied: false,
            skipped: true,
            decision,
            apply_result: None,
        };
    }

    let key = auto_heal_key(&decision.provider, decision.channel_id, decision.action);
    match reserve_auto_heal_attempt(&key, now_ms) {
        Ok(remaining) => {
            decision.auto_heal.remaining_attempts = remaining;
        }
        Err(reason) => {
            decision.auto_heal.remaining_attempts = 0;
            decision.auto_heal.skipped_reason = Some(reason);
            trace_relay_recovery_skipped(&decision, Some(reason));
            return RelayRecoveryResponse {
                ok: false,
                mode: "apply",
                applied: false,
                skipped: true,
                decision,
                apply_result: None,
            };
        }
    }

    let apply_result =
        apply_relay_recovery_decision(registry, shared, provider, &decision, source).await;
    let applied = relay_recovery_status_counts_as_applied(apply_result.status);
    tracing::info!(
        target: "agentdesk::discord::relay_recovery",
        provider = decision.provider.as_str(),
        channel_id = decision.channel_id,
        action = decision.action.as_str(),
        source = source.as_str(),
        status = apply_result.status,
        removed_thread_proofs = apply_result.removed_thread_proofs,
        removed_mailbox_token = apply_result.removed_mailbox_token,
        "relay recovery auto-heal applied"
    );
    RelayRecoveryResponse {
        ok: applied,
        mode: "apply",
        applied,
        skipped: false,
        decision,
        apply_result: Some(apply_result),
    }
}

fn relay_recovery_status_counts_as_applied(status: &'static str) -> bool {
    matches!(
        status,
        "applied"
            | "reattached_watcher"
            | "reuse_existing_live_watcher"
            | "cleared_idle_tmux_stale_turn"
    )
}

/// #3277 verify-2 (Defect D follow-up): report the reattach apply HONESTLY.
/// `rebind_inflight_for_channel` routes through the single-watcher claim
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

fn forget_completion_footer_for_relay_recovery(channel_id: ChannelId) {
    super::single_message_panel::completion_footer_forget_registered_target(channel_id);
}

fn idle_tmux_repair_ready_for_input(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session: &str,
) -> bool {
    let structured_ready =
        super::inflight::load_inflight_state(provider, channel_id).and_then(|state| {
            let output_path = state
                .output_path
                .as_deref()
                .map(str::trim)
                .filter(|path| !path.is_empty())?;
            crate::services::tui_turn_state::jsonl_ready_for_input(
                provider,
                state.runtime_kind,
                std::path::Path::new(output_path),
                Some(state.last_offset),
            )
        });
    structured_ready
        .map(crate::services::tui_turn_state::TuiReadyState::is_ready)
        .unwrap_or_else(|| {
            crate::services::provider::tmux_session_ready_for_input(tmux_session, provider)
        })
}

async fn apply_relay_recovery_decision(
    registry: &HealthRegistry,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    decision: &RelayRecoveryDecision,
    source: RelayRecoveryApplySource,
) -> RelayRecoveryApplyResult {
    match decision.action {
        RelayRecoveryActionKind::ClearStaleThreadProof => {
            let channel = ChannelId::new(decision.channel_id);
            let before = shared.dispatch.thread_parents.len();
            shared
                .dispatch
                .thread_parents
                .retain(|parent, thread| *parent != channel && *thread != channel);
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
            forget_completion_footer_for_relay_recovery(channel);
            if let Some(tmux_session) = decision.affected.tmux_session.as_deref()
                && idle_tmux_repair_ready_for_input(provider, decision.channel_id, tmux_session)
                && super::inflight::inflight_state_allows_idle_tmux_repair(
                    provider,
                    decision.channel_id,
                )
                .unwrap_or(false)
            {
                let finish = mailbox_finish_turn(shared, provider, channel).await;
                if let Some(token) = finish.removed_token.as_ref() {
                    token.cancelled.store(true, Ordering::Relaxed);
                    super::saturating_decrement_global_active(shared);
                }
                super::clear_watchdog_deadline_override(channel.get()).await;
                shared
                    .dispatch
                    .thread_parents
                    .retain(|_, thread| *thread != channel);
                shared.restart.recovering_channels.remove(&channel);
                shared.turn_start_times.remove(&channel);
                if !finish.has_pending {
                    shared.dispatch.role_overrides.remove(&channel);
                }
                if let Some((_, watcher)) = shared.tmux_watchers.remove(&channel) {
                    watcher.cancel.store(true, Ordering::Relaxed);
                }
                let inflight_cleared =
                    super::inflight::clear_inflight_state(provider, decision.channel_id);
                mailbox_clear_recovery_marker(shared, channel).await;
                let after = mailbox_snapshot(shared, channel).await;
                return RelayRecoveryApplyResult {
                    status: "cleared_idle_tmux_stale_turn",
                    removed_thread_proofs: 0,
                    removed_mailbox_token: finish.removed_token.is_some(),
                    post_mailbox_has_cancel_token: Some(after.cancel_token.is_some()),
                    post_mailbox_queue_depth: Some(after.intervention_queue.len()),
                    reattach_watcher_spawned: Some(false),
                    reattach_watcher_replaced: Some(inflight_cleared),
                    reattach_initial_offset: None,
                    reattach_error: None,
                };
            }
            match registry
                .rebind_inflight(
                    provider,
                    decision.channel_id,
                    decision.affected.tmux_session.clone(),
                )
                .await
            {
                Some(Ok(outcome)) => RelayRecoveryApplyResult {
                    status: reattach_apply_status(outcome.watcher_spawned),
                    removed_thread_proofs: 0,
                    removed_mailbox_token: false,
                    post_mailbox_has_cancel_token: None,
                    post_mailbox_queue_depth: None,
                    reattach_watcher_spawned: Some(outcome.watcher_spawned),
                    reattach_watcher_replaced: Some(outcome.watcher_replaced),
                    reattach_initial_offset: Some(outcome.initial_offset),
                    reattach_error: None,
                },
                Some(Err(error)) => RelayRecoveryApplyResult {
                    status: "rebind_failed",
                    removed_thread_proofs: 0,
                    removed_mailbox_token: false,
                    post_mailbox_has_cancel_token: None,
                    post_mailbox_queue_depth: None,
                    reattach_watcher_spawned: None,
                    reattach_watcher_replaced: None,
                    reattach_initial_offset: None,
                    reattach_error: Some(error.to_string()),
                },
                None => RelayRecoveryApplyResult {
                    status: "provider_unavailable",
                    removed_thread_proofs: 0,
                    removed_mailbox_token: false,
                    post_mailbox_has_cancel_token: None,
                    post_mailbox_queue_depth: None,
                    reattach_watcher_spawned: None,
                    reattach_watcher_replaced: None,
                    reattach_initial_offset: None,
                    reattach_error: Some("provider unavailable".to_string()),
                },
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
fn clear_auto_heal_attempts_for_tests() {
    auto_heal_attempts()
        .lock()
        .expect("relay recovery attempt map poisoned")
        .clear();
}

#[cfg(test)]
mod tests {
    use super::super::relay_health::RelayStallClassifier;
    use super::*;
    use crate::services::provider::{CancelToken, ProviderKind};
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, OnceLock};

    fn auto_heal_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
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
        super::super::single_message_panel::completion_footer_forget_registered_target(channel_id);
        let _ = super::super::single_message_panel::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_303),
            &ProviderKind::Codex,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );

        forget_completion_footer_for_relay_recovery(channel_id);

        assert_eq!(
            super::super::single_message_panel::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                1_800_000_005,
            ),
            None
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
            AUTO_HEAL_MAX_ATTEMPTS_PER_WINDOW
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
    fn queue_blocked_is_honest_observe_only_because_health_already_degrades() {
        let decision = plan_relay_recovery(
            &RelayHealthSnapshot {
                queue_depth: 2,
                ..snapshot()
            },
            RelayStallState::QueueBlocked,
            1_000,
        );

        assert_eq!(decision.action, RelayRecoveryActionKind::ObserveOnly);
        assert_eq!(
            decision.reason,
            "queued work is already surfaced as degraded health; relay recovery has no safe local cleanup"
        );
        assert!(!decision.auto_heal.eligible);
        assert_eq!(
            decision.auto_heal.skipped_reason,
            Some("operator_inspection_required")
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
    fn live_agentdesk_tmux_relay_dead_without_mailbox_token_needs_operator() {
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
        assert!(!decision.auto_heal.eligible);
        assert_eq!(
            decision.auto_heal.skipped_reason,
            Some("reattach_missing_required_live_evidence")
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
        );

        assert_eq!(reserve_auto_heal_attempt(&key, 1_000), Ok(0));
        assert_eq!(
            reserve_auto_heal_attempt(&key, 2_000),
            Err("auto_heal_rate_limited")
        );
        assert_eq!(
            reserve_auto_heal_attempt(&key, 1_000 + AUTO_HEAL_WINDOW_SECS * 1000),
            Ok(0)
        );
    }

    #[tokio::test]
    async fn auto_apply_orphan_pending_token_clears_mailbox_token() {
        let _guard = auto_heal_test_lock().lock().await;
        clear_auto_heal_attempts_for_tests();
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
}
