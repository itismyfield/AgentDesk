use super::*;

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
