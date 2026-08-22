use super::*;

// #5071 T4-B6: `health::reachability` is `#[cfg(unix)]`, so the reachability
// operand and everything typed by it — this import,
// `plan_relay_recovery_under_reachability` and its authority tests — is gated
// the same way. No production path calls that planner in either configuration;
// windows simply keeps only the structural `plan_relay_recovery`.
#[cfg(unix)]
use crate::services::discord::health::reachability::verdict::{
    NotAliveObligationState, ReachabilityUnknownReason, ReachabilityVerdict,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum RelayRecoveryActionKind {
    ObserveOnly,
    ClearStaleThreadProof,
    ClearOrphanPendingToken,
    ReattachWatcher,
    DrainPendingQueue,
    /// #5071 T4-B6 (4987 §4.4 / §7.1): the relay looks unreachable while the
    /// structural signals still read as a live foreground stream. Observation
    /// with a distinct label, so the operator sees the contradiction; it
    /// touches nothing and is never auto-heal eligible.
    ReportRelayUnreachable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum RelayRecoveryApplySource {
    Manual,
    ProbeAutoHeal,
    StallWatchdog,
}

impl RelayRecoveryApplySource {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::ProbeAutoHeal => "probe_auto_heal",
            Self::StallWatchdog => "stall_watchdog",
        }
    }

    pub(super) fn finalizer_reason(self) -> &'static str {
        match self {
            Self::StallWatchdog => "1446_stall_watchdog",
            Self::Manual | Self::ProbeAutoHeal => "1462_relay_recovery_auto_heal",
        }
    }

    pub(super) fn cleanup_session(self) -> bool {
        matches!(self, Self::StallWatchdog)
    }
}

impl RelayRecoveryActionKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::ObserveOnly => "observe_only",
            Self::ClearStaleThreadProof => "clear_stale_thread_proof",
            Self::ClearOrphanPendingToken => "clear_orphan_pending_token",
            Self::ReattachWatcher => "reattach_watcher",
            Self::DrainPendingQueue => "drain_pending_queue",
            Self::ReportRelayUnreachable => "report_relay_unreachable",
        }
    }

    /// Whether applying this action mutates runtime state a live turn depends
    /// on — 4987 §7.1's destructive set as this planner's actions map onto it.
    ///
    /// Conservative on purpose. `ReattachWatcher` and `DrainPendingQueue` are
    /// counted as destructive because their `relay_recovery::apply` arms both
    /// take effect on runtime state — a watcher respawn/rebind and a scheduled
    /// queue drain — even though neither cancels a turn by itself. False is
    /// reserved for the two arms whose apply path returns `"skipped"` and
    /// writes nothing. The arms are spelled out rather than collapsed so a new
    /// action has to choose a side before it compiles.
    ///
    /// This classifies the ACTION, not the apply path's own guards: an action
    /// marked destructive here is still subject to every eligibility and budget
    /// check in `relay_recovery::apply`.
    pub(in crate::services::discord) fn is_destructive(self) -> bool {
        match self {
            Self::ObserveOnly | Self::ReportRelayUnreachable => false,
            Self::ClearStaleThreadProof
            | Self::ClearOrphanPendingToken
            | Self::ReattachWatcher
            | Self::DrainPendingQueue => true,
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
    pub mailbox_turn_started_at_ms: Option<i64>,
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

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum AxisBDiff {
    Agree,
    LedgerMilder,
    LedgerStricter,
}

#[cfg(unix)]
impl AxisBDiff {
    /// The ledger planner is monotone-relaxing: it may retain or remove a
    /// structural action, never introduce a new destructive one.
    pub(in crate::services::discord) fn from_decisions(
        structural: &RelayRecoveryDecision,
        ledger: &RelayRecoveryDecision,
    ) -> Self {
        Self::from_outcomes(
            structural.action,
            structural.auto_heal.eligible,
            ledger.action,
            ledger.auto_heal.eligible,
        )
    }

    pub(in crate::services::discord) fn from_outcomes(
        structural_action: RelayRecoveryActionKind,
        structural_eligible: bool,
        ledger_action: RelayRecoveryActionKind,
        ledger_eligible: bool,
    ) -> Self {
        if structural_action == ledger_action && structural_eligible == ledger_eligible {
            Self::Agree
        } else if (ledger_action.is_destructive() && ledger_action != structural_action)
            || (ledger_eligible && !structural_eligible)
        {
            Self::LedgerStricter
        } else {
            Self::LedgerMilder
        }
    }

    pub(in crate::services::discord) fn preserves_monotone_relaxation(self) -> bool {
        self != Self::LedgerStricter
    }
}

#[cfg(unix)]
pub(in crate::services::discord) fn reachability_unknown_reason_label(
    verdict: &ReachabilityVerdict,
) -> Option<&'static str> {
    Some(match verdict.unknown_reason()? {
        ReachabilityUnknownReason::TranscriptUnresolved => "transcript_unresolved",
        ReachabilityUnknownReason::NeverObserved => "never_observed",
        ReachabilityUnknownReason::ProviderUnresolved => "provider_unresolved",
        ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
            NotAliveObligationState::NoneOutstanding,
        ) => "incarnation_not_alive_no_obligations",
        ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
            NotAliveObligationState::WithinGrace,
        ) => "incarnation_not_alive_within_grace",
        ReachabilityUnknownReason::TranscriptCoordinateDivergence => {
            "transcript_coordinate_divergence"
        }
        ReachabilityUnknownReason::RowlessActiveTurn => "rowless_active_turn",
        ReachabilityUnknownReason::ReadTruncated => "read_truncated",
        ReachabilityUnknownReason::ReceiptStoreUnreadable => "receipt_store_unreadable",
    })
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

pub(super) fn is_agentdesk_tmux_session(tmux_session: Option<&str>) -> bool {
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
        mailbox_turn_started_at_ms: snapshot.mailbox_turn_started_at_ms,
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

fn orphan_pending_token_within_admission_grace(
    snapshot: &RelayHealthSnapshot,
    now_ms: i64,
) -> bool {
    snapshot
        .mailbox_turn_started_at_ms
        .is_some_and(|started_at_ms| {
            now_ms.saturating_sub(started_at_ms)
                < ORPHAN_PENDING_TOKEN_ADMISSION_GRACE.as_millis() as i64
        })
}

pub(super) fn eligible_orphan_pending_token_without_admission_grace(
    snapshot: &RelayHealthSnapshot,
) -> bool {
    snapshot.mailbox_has_cancel_token
        && !snapshot.bridge_inflight_present
        && !snapshot.watcher_attached
        && snapshot.tmux_alive != Some(true)
        // The AgentDesk-name guard only protects a token whose tmux liveness is
        // still uncertain (`None`, e.g. a transient probe error) — NOT one the
        // probe positively confirmed dead. Without the `Some(false)` escape a
        // genuinely dead `AgentDesk-*` orphan token is protected forever and
        // wedges the mailbox slot with no reclaim path (#4569 review regression).
        && (snapshot.tmux_alive == Some(false)
            || !is_agentdesk_tmux_session(snapshot.tmux_session.as_deref()))
}

fn eligible_orphan_pending_token(snapshot: &RelayHealthSnapshot, now_ms: i64) -> bool {
    eligible_orphan_pending_token_without_admission_grace(snapshot)
        && !orphan_pending_token_within_admission_grace(snapshot, now_ms)
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
        && (snapshot.watcher_binding_is_not_a_live_relay_owner()
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
            let eligible = eligible_orphan_pending_token(snapshot, now_ms);
            let admission_grace = orphan_pending_token_within_admission_grace(snapshot, now_ms);
            (
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                "mailbox holds a cancel token without bridge, watcher, or live tmux evidence",
                eligible,
                (!eligible).then_some(if protected_tmux {
                    "protected_agentdesk_tmux_session"
                } else if snapshot.bridge_inflight_present
                    || snapshot.watcher_attached
                    || snapshot.tmux_alive == Some(true)
                {
                    "orphan_token_has_live_evidence"
                } else if admission_grace {
                    "orphan_token_within_admission_grace"
                } else {
                    "orphan_token_missing_required_evidence"
                }),
            )
        }
        RelayStallState::UnpairedActiveToken => (
            RelayRecoveryActionKind::ObserveOnly,
            "active mailbox token remains unpaired after read-side confirmation",
            false,
            Some("unpaired_active_token_observe_only"),
        ),
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

/// 4987 §4.4's `(RelayStallState, ReachabilityVerdict)` planner — #5071 T4-B6.
///
/// Structured as an override on top of [`plan_relay_recovery`] rather than as a
/// second decision tree, and the override has exactly one direction: it may
/// replace the structural action with a NON-destructive one and drop
/// eligibility. It never selects an action the structural planner did not, and
/// it never raises eligibility. That shape is what makes the 4987 §7.1 / I15
/// rule — reachability authorizes no destructive action — a property of the
/// function instead of a property of the arms one at a time, and
/// `reachability_never_selects_or_enables_a_destructive_action` asserts it over
/// the full cross product.
///
/// The only combination that currently changes anything is 4987 §4.4's named
/// one: a live-looking foreground stream whose relay verdict is `Unreachable`.
/// That is the #4986 형상1 contradiction — the structural signals report a
/// healthy stream while no obligation is being covered — and it earns a label,
/// not a cleanup.
///
/// **No production path calls this yet.** T4-B6 lands the composed verdict on
/// the health surface (`health::snapshot`) and this planner beside it; routing
/// the recovery entry points in `relay_recovery` through it needs a reachability
/// operand at those call sites, which is a later slice. Until then the runtime
/// keeps calling [`plan_relay_recovery`] and the reachability tier changes no
/// recovery action at all.
#[cfg(unix)]
pub(in crate::services::discord) fn plan_relay_recovery_under_reachability(
    snapshot: &RelayHealthSnapshot,
    relay_stall_state: RelayStallState,
    reachability: &ReachabilityVerdict,
    now_ms: i64,
) -> RelayRecoveryDecision {
    let mut decision = plan_relay_recovery(snapshot, relay_stall_state, now_ms);
    let contradiction = matches!(relay_stall_state, RelayStallState::ActiveForegroundStream)
        && matches!(reachability, ReachabilityVerdict::Unreachable { .. });
    if contradiction {
        decision.action = RelayRecoveryActionKind::ReportRelayUnreachable;
        decision.reason =
            "foreground stream looks live while no relay obligation is covered; observation only";
        decision.auto_heal.eligible = false;
        decision.auto_heal.skipped_reason = Some("reachability_observation_only");
    }
    decision
}

#[cfg(all(test, unix))]
mod reachability_authority_tests {
    use super::*;
    use crate::services::discord::health::reachability::verdict::{
        ReachabilityUnknownReason, TransportUnknownEvidence,
    };

    fn every_stall_state() -> [RelayStallState; 8] {
        [
            RelayStallState::Healthy,
            RelayStallState::ActiveForegroundStream,
            RelayStallState::ExplicitBackgroundWork,
            RelayStallState::TmuxAliveRelayDead,
            RelayStallState::StaleThreadProof,
            RelayStallState::OrphanPendingToken,
            RelayStallState::UnpairedActiveToken,
            RelayStallState::QueueBlocked,
        ]
    }

    fn every_verdict() -> Vec<ReachabilityVerdict> {
        vec![
            ReachabilityVerdict::Reachable,
            ReachabilityVerdict::Degraded {
                oldest_unsatisfied_age_secs: 300,
                uncovered_ranges: 2,
            },
            ReachabilityVerdict::TransportUnknown {
                since_secs: 700,
                evidence: TransportUnknownEvidence::RestartBoundaryCrossed,
            },
            ReachabilityVerdict::TransportUnknown {
                since_secs: 700,
                evidence: TransportUnknownEvidence::PlaceholderPresent,
            },
            ReachabilityVerdict::TransportUnknown {
                since_secs: 700,
                evidence: TransportUnknownEvidence::UnreleasedDeliveryLease,
            },
            ReachabilityVerdict::Unreachable {
                oldest_unsatisfied_age_secs: 900,
                uncovered_ranges: 4,
            },
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::TranscriptUnresolved, 30),
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::NeverObserved, 30),
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::ProviderUnresolved, 30),
            ReachabilityVerdict::unknown(
                ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                    NotAliveObligationState::NoneOutstanding,
                ),
                30,
            ),
            ReachabilityVerdict::unknown(
                ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                    NotAliveObligationState::WithinGrace,
                ),
                30,
            ),
            ReachabilityVerdict::unknown(
                ReachabilityUnknownReason::TranscriptCoordinateDivergence,
                30,
            ),
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::RowlessActiveTurn, 30),
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::ReadTruncated, 30),
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::ReceiptStoreUnreadable, 30),
        ]
    }

    /// A snapshot deliberately shaped so the structural planner finds live
    /// evidence everywhere: every destructive branch's eligibility predicate
    /// reads this as "a turn is running", so any destructive action or raised
    /// eligibility appearing in the reachability-aware planner came from the
    /// reachability operand and nothing else.
    fn live_snapshot() -> RelayHealthSnapshot {
        let mut snapshot = quiet_snapshot();
        snapshot.tmux_session = Some("agentdesk-codex-42".to_string());
        snapshot.tmux_alive = Some(true);
        snapshot.watcher_attached = true;
        snapshot.watcher_owns_live_relay = true;
        snapshot.bridge_inflight_present = true;
        snapshot.mailbox_has_cancel_token = true;
        snapshot.active_turn = RelayActiveTurn::Foreground;
        snapshot
    }

    /// The mirror shape: nothing live anywhere, so the structural planner takes
    /// its most permissive branches. Running the cross product over both ends
    /// is what makes "reachability never enables anything" a claim about the
    /// override rather than about one convenient fixture.
    fn quiet_snapshot() -> RelayHealthSnapshot {
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
        }
    }

    /// The I15 mutation lock. Adding any branch that lets a reachability
    /// verdict pick a destructive action — or re-enable one the structural
    /// planner refused — fails here, for every (stall state, verdict) pair.
    #[test]
    fn reachability_never_selects_or_enables_a_destructive_action() {
        let stricter = AxisBDiff::from_outcomes(
            RelayRecoveryActionKind::ObserveOnly,
            false,
            RelayRecoveryActionKind::ReattachWatcher,
            true,
        );
        let eligibility_only_stricter = AxisBDiff::from_outcomes(
            RelayRecoveryActionKind::ObserveOnly,
            false,
            RelayRecoveryActionKind::ObserveOnly,
            true,
        );
        let milder = AxisBDiff::from_outcomes(
            RelayRecoveryActionKind::ReattachWatcher,
            true,
            RelayRecoveryActionKind::ObserveOnly,
            false,
        );
        let agree = AxisBDiff::from_outcomes(
            RelayRecoveryActionKind::ReattachWatcher,
            true,
            RelayRecoveryActionKind::ReattachWatcher,
            true,
        );
        assert_eq!(stricter, AxisBDiff::LedgerStricter);
        assert_eq!(eligibility_only_stricter, AxisBDiff::LedgerStricter);
        assert_eq!(milder, AxisBDiff::LedgerMilder);
        assert_eq!(agree, AxisBDiff::Agree);
        assert!(!stricter.preserves_monotone_relaxation());
        assert!(milder.preserves_monotone_relaxation());
        assert!(agree.preserves_monotone_relaxation());
        let unknown_labels: Vec<_> = every_verdict()
            .iter()
            .filter_map(reachability_unknown_reason_label)
            .collect();
        assert_eq!(
            unknown_labels,
            [
                "transcript_unresolved",
                "never_observed",
                "provider_unresolved",
                "incarnation_not_alive_no_obligations",
                "incarnation_not_alive_within_grace",
                "transcript_coordinate_divergence",
                "rowless_active_turn",
                "read_truncated",
                "receipt_store_unreadable",
            ]
        );
        for snapshot in [quiet_snapshot(), live_snapshot()] {
            for stall_state in every_stall_state() {
                let structural = plan_relay_recovery(&snapshot, stall_state, 1_000);
                for verdict in every_verdict() {
                    let composed = plan_relay_recovery_under_reachability(
                        &snapshot,
                        stall_state,
                        &verdict,
                        1_000,
                    );
                    let diff = AxisBDiff::from_decisions(&structural, &composed);
                    assert!(
                        diff.preserves_monotone_relaxation(),
                        "ledger became stricter for {stall_state:?}/{verdict:?}"
                    );
                    if composed.action.is_destructive() {
                        assert_eq!(
                            composed.action, structural.action,
                            "reachability introduced destructive {:?} for {stall_state:?}/{verdict:?}",
                            composed.action
                        );
                    }
                    assert!(
                        !(composed.auto_heal.eligible && !structural.auto_heal.eligible),
                        "reachability raised auto-heal eligibility for {stall_state:?}/{verdict:?}"
                    );
                    assert!(
                        !verdict.authorizes_destructive_action(),
                        "no reachability verdict may claim destructive authority"
                    );
                }
            }
        }
    }

    /// 4987 §4.4's named combination, and its blast radius: only this pair
    /// changes, and what it changes to writes nothing.
    #[test]
    fn foreground_stream_with_unreachable_relay_reports_without_acting() {
        let snapshot = live_snapshot();
        let unreachable = ReachabilityVerdict::Unreachable {
            oldest_unsatisfied_age_secs: 900,
            uncovered_ranges: 4,
        };
        let decision = plan_relay_recovery_under_reachability(
            &snapshot,
            RelayStallState::ActiveForegroundStream,
            &unreachable,
            1_000,
        );
        assert_eq!(
            decision.action,
            RelayRecoveryActionKind::ReportRelayUnreachable
        );
        assert!(!decision.action.is_destructive());
        assert!(!decision.auto_heal.eligible);

        // Every other stall state keeps the structural answer under the same
        // verdict, so this override is one pair wide and not a general
        // reachability veto.
        for stall_state in every_stall_state() {
            if matches!(stall_state, RelayStallState::ActiveForegroundStream) {
                continue;
            }
            assert_eq!(
                plan_relay_recovery_under_reachability(&snapshot, stall_state, &unreachable, 1_000)
                    .action,
                plan_relay_recovery(&snapshot, stall_state, 1_000).action,
                "{stall_state:?} must keep its structural action"
            );
        }
    }
}
