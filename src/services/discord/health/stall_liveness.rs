use std::sync::LazyLock;

use poise::serenity_prelude::ChannelId;

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::relay_health::RelayActiveTurn;
use crate::services::provider::ProviderKind;

use super::snapshot::WatcherStateSnapshot;

pub(super) const STALL_WATCHDOG_POSITIVE_LIVENESS_SECS: u64 = 120;
/// Historical deferral-budget field for force-clean deferrals while positive
/// liveness keeps being observed. A deferral only ever fires when
/// `has_positive_liveness` is true: fresh bytes are demonstrably flowing (pane or
/// relay offset advanced cross-tick, transcript/runtime jsonl mtime inside
/// `POSITIVE_LIVENESS_SECS`, or a fresh background-synthetic anchor), or an
/// undelivered backlog is still inside the short no-progress observation grace
/// used to prove whether it is draining. Once that backlog grace expires without
/// relay-offset progress, `reason_codes == none` and the first eligible cleanup
/// tick proceeds instead of waiting for the absolute backstop.
///
/// #3582: raised 3 -> 20. At the old value a *live* turn that kept emitting output
/// for longer than `THRESHOLD_SECS + 3 * INTERVAL_SECS` (~600s + ~90s) was killed
/// mid-stream the instant the cap was hit even though `reason_codes` still listed
/// `pane_offset_advanced_recently,transcript_mtime_recent` — the confirmed
/// 2026-06-18 12:07 false-positive (a "Response sent" landed 5s after the
/// force-clean). The window is only ~90s of grace over the threshold, far short of
/// a long but live turn.
///
/// #3671: a ~40-minute single turn (a release self-deploy that recompiled the whole
/// tree) survived a mid-turn SIGTERM restart, was preserved by drain_restart, and
/// kept showing positive liveness (pane offset advancing, fresh transcript mtime)
/// — yet the 20-tick cap (`20 * INTERVAL_SECS` ~= 600s of grace) was still reached
/// and force-cleaned a demonstrably *live* turn. A tick-count ceiling is a brittle
/// proxy for wall-clock (ticks drift when the interval changes or ticks are
/// skipped), so the cleanup gate is no longer the tick count. While positive
/// liveness keeps being observed the force-clean is deferred indefinitely; the
/// finite detection ceiling required by #3582 R1 is now an *age*-based absolute
/// backstop (`STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS`) measured against the turn's
/// real invariant — its age. `STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS` is retained
/// only as log context (`max_deferrals`); positive liveness no longer consumes or
/// preserves a cleanup escalation budget.
pub(super) const STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS: u8 = 20;
/// Absolute, age-based detection ceiling for the stall watchdog. While positive
/// liveness is observed a force-clean is deferred indefinitely *up to* this bound;
/// once the in-flight turn's age (anchored at `started_at.max(boot)`, the same
/// anchor `StallWatchdogJudgmentBasis::from_snapshot` uses) reaches it, a
/// genuine forever-spinner (pane bytes flow but no answer ever lands) is
/// force-cleaned. This keeps the detection ceiling finite as #3582 R1 requires.
///
/// Aligned to the 4h Codex per-turn hard ceiling
/// (`codex_tmux_wrapper::DEFAULT_CODEX_TURN_HARD_CEILING_SECS`) and well above any
/// legitimate turn measured from the post-restart anchor. The anchor resets to
/// `boot` on restart (#3557), so a turn that survives a restart is re-granted a
/// full 4h window from the restart instant — the ~40-minute #3671 deploy turn
/// sits far below this bound and is never killed while live. This watchdog-level
/// backstop is independent of the process-level hard ceiling (Codex 4h /
/// other providers 6h via `AGENTDESK_TURN_HARD_CEILING_SECS`): defense in depth,
/// so the watchdog alone still guarantees a finite ceiling even if the process
/// ceiling is overridden away.
pub(super) const STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS: u64 = 4 * 3600;
pub(super) const STALL_LIVENESS_STATE_TTL_SECS: u64 = 1800;
pub(super) const STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS: u64 = 180;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum OffsetObservationKind {
    PaneCapture,
    RelayDelivered,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct StallLivenessKey {
    offset_kind: OffsetObservationKind,
    provider: String,
    channel_id: u64,
    tmux_session: Option<String>,
    user_msg_id: Option<u64>,
    started_at: Option<String>,
}

#[derive(Clone, Debug)]
struct OffsetObservation {
    offset: u64,
    advanced_at_unix_secs: Option<i64>,
    unchanged_since_unix_secs: i64,
    last_updated_unix_secs: i64,
}

#[derive(Clone, Debug, Default)]
struct OffsetObservationResult {
    previous_offset: Option<u64>,
    advanced_age_secs: Option<u64>,
    unchanged_age_secs: Option<u64>,
}

static OFFSET_OBSERVATIONS: LazyLock<dashmap::DashMap<StallLivenessKey, OffsetObservation>> =
    LazyLock::new(dashmap::DashMap::new);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StallWatchdogLivenessAction {
    ProceedNoEvidence,
    Defer { deferral_count: u8 },
    ProceedAfterAbsoluteBackstop { age_secs: u64, deferral_count: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StallWatchdogLivenessDecision {
    pub(super) action: StallWatchdogLivenessAction,
    pub(super) evidence: StallWatchdogLivenessEvidence,
    pub(super) max_deferrals: u8,
}

impl StallWatchdogLivenessDecision {
    pub(super) fn should_defer(&self) -> bool {
        matches!(self.action, StallWatchdogLivenessAction::Defer { .. })
    }

    fn deferral_count(&self) -> Option<u8> {
        match self.action {
            StallWatchdogLivenessAction::Defer { deferral_count } => Some(deferral_count),
            StallWatchdogLivenessAction::ProceedAfterAbsoluteBackstop {
                deferral_count, ..
            } => Some(deferral_count),
            StallWatchdogLivenessAction::ProceedNoEvidence => None,
        }
    }

    fn absolute_backstop_reached(&self) -> bool {
        matches!(
            self.action,
            StallWatchdogLivenessAction::ProceedAfterAbsoluteBackstop { .. }
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct StallWatchdogLivenessEvidence {
    pub(super) pane_offset_current: Option<u64>,
    pub(super) pane_offset_previous: Option<u64>,
    pub(super) pane_offset_advanced_age_secs: Option<u64>,
    pub(super) relay_offset_current: Option<u64>,
    pub(super) relay_offset_previous: Option<u64>,
    pub(super) relay_offset_advanced_age_secs: Option<u64>,
    pub(super) transcript_mtime_age_secs: Option<u64>,
    pub(super) runtime_activity_age_secs: Option<u64>,
    pub(super) outbound_activity_age_secs: Option<u64>,
    pub(super) background_synthetic_activity_age_secs: Option<u64>,
    pub(super) background_synthetic_kind: Option<String>,
    pub(super) has_undelivered_backlog: bool,
}

impl StallWatchdogLivenessEvidence {
    fn reason_codes(&self, freshness_secs: u64) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if is_recent_age(self.pane_offset_advanced_age_secs, freshness_secs) {
            reasons.push("pane_offset_advanced_recently");
        }
        if is_recent_age(self.relay_offset_advanced_age_secs, freshness_secs) {
            reasons.push("relay_offset_advanced_recently");
        }
        if is_recent_age(self.transcript_mtime_age_secs, freshness_secs) {
            reasons.push("transcript_mtime_recent");
        }
        if is_recent_age(self.runtime_activity_age_secs, freshness_secs) {
            reasons.push("runtime_activity_mtime_recent");
        }
        if is_recent_age(self.outbound_activity_age_secs, freshness_secs) {
            reasons.push("outbound_activity_recent");
        }
        if is_recent_age(self.background_synthetic_activity_age_secs, freshness_secs) {
            reasons.push("background_synthetic_activity_recent");
        }
        if self.has_undelivered_backlog {
            reasons.push("has_undelivered_backlog");
        }
        reasons
    }

    fn reason_codes_csv(&self, freshness_secs: u64) -> String {
        let reasons = self.reason_codes(freshness_secs);
        if reasons.is_empty() {
            "none".to_string()
        } else {
            reasons.join(",")
        }
    }

    fn has_positive_liveness(&self, freshness_secs: u64) -> bool {
        !self.reason_codes(freshness_secs).is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StallWatchdogJudgmentBasis {
    /// Age at the `started_at.max(boot)` anchor — used by the initial destructive
    /// `should_clean` threshold gate so a turn freshly recovered across a restart
    /// gets a post-boot grace window before it is reconsidered for cleanup.
    pub(super) inflight_age_secs: Option<u64>,
    pub(super) inflight_age_anchor_unix_secs: Option<i64>,
    /// Raw age from `started_at` with NO boot floor — the turn's true wall-clock
    /// age, invariant across dcserver restarts. The absolute backstop measures
    /// this so repeated restarts cannot reset the finite detection ceiling (#3671).
    pub(super) turn_age_secs: Option<u64>,
    pub(super) last_relay_age_secs: Option<u64>,
    pub(super) last_outbound_activity_age_secs: Option<u64>,
}

impl StallWatchdogJudgmentBasis {
    pub(super) fn from_snapshot(
        snapshot: &WatcherStateSnapshot,
        now_unix_secs: i64,
        boot_unix_secs: i64,
    ) -> Self {
        let started_at_unix = snapshot
            .inflight_started_at
            .as_deref()
            .and_then(crate::services::discord::inflight::parse_updated_at_unix);
        let inflight_age_anchor_unix_secs =
            started_at_unix.map(|started| started.max(boot_unix_secs));
        Self {
            inflight_age_secs: inflight_age_anchor_unix_secs
                .map(|anchor| saturating_age_secs(anchor, now_unix_secs)),
            inflight_age_anchor_unix_secs,
            turn_age_secs: started_at_unix
                .map(|started| saturating_age_secs(started, now_unix_secs)),
            last_relay_age_secs: unix_millis_age_secs(
                positive_millis(snapshot.last_relay_ts_ms),
                now_unix_secs,
            ),
            last_outbound_activity_age_secs: unix_millis_age_secs(
                snapshot.relay_health.last_outbound_activity_ms,
                now_unix_secs,
            ),
        }
    }
}

pub(super) fn evaluate_stall_watchdog_liveness(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    inflight: Option<&InflightTurnState>,
    now_unix_secs: i64,
    freshness_secs: u64,
    max_deferrals: u8,
    backstop_age_secs: Option<u64>,
) -> StallWatchdogLivenessDecision {
    let key = StallLivenessKey::from_snapshot(provider, channel_id, snapshot);
    let evidence = StallWatchdogLivenessEvidence::collect(&key, snapshot, inflight, now_unix_secs);
    if !evidence.has_positive_liveness(freshness_secs) {
        // A genuinely dead relay (every signal stale ⇒ reason_codes == none) is
        // cleaned on the very first tick, untouched by the deferral state or the
        // absolute backstop. This branch is invariant (#3582 / #3671).
        return StallWatchdogLivenessDecision {
            action: StallWatchdogLivenessAction::ProceedNoEvidence,
            evidence,
            max_deferrals,
        };
    }

    // #3671: positive liveness defers indefinitely up to the age-based absolute
    // backstop. The backstop is the only cleanup gate now — positive evidence
    // resets the cleanup escalation budget instead of consuming it.
    // `backstop_age_secs` is the turn's RAW age from `started_at`
    // (`StallWatchdogJudgmentBasis::turn_age_secs`), with NO boot floor — so a
    // forever-spinner cannot reset the finite detection ceiling by surviving
    // repeated dcserver restarts (each restart only re-arms the post-boot grace
    // on the separate `should_clean` threshold gate, which uses the boot-floored
    // `inflight_age_secs`). A ~40-minute deploy turn stays far below the 4h
    // ceiling regardless of how many restarts it rode through. When the age is
    // unknown (no started_at) the backstop cannot fire; that only matters under
    // positive liveness, which is abnormal without a started_at and is still
    // bounded by the process-level hard ceiling killing the pane (next tick takes
    // the ProceedNoEvidence branch above).
    if backstop_age_secs.is_some_and(|age| age >= STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS) {
        return StallWatchdogLivenessDecision {
            action: StallWatchdogLivenessAction::ProceedAfterAbsoluteBackstop {
                age_secs: backstop_age_secs.unwrap_or(0),
                deferral_count: 0,
            },
            evidence,
            max_deferrals,
        };
    }

    StallWatchdogLivenessDecision {
        action: StallWatchdogLivenessAction::Defer { deferral_count: 0 },
        evidence,
        max_deferrals,
    }
}

pub(super) fn clear_stall_watchdog_liveness_state(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session: Option<&str>,
) {
    let probe = StallLivenessKey::new(provider, channel_id, tmux_session, None, None);
    OFFSET_OBSERVATIONS.retain(|key, _| !key.matches_session(&probe));
}

pub(super) fn clear_stall_watchdog_liveness_state_if_healthy(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
) -> bool {
    if !stall_watchdog_liveness_state_is_healthy(snapshot) {
        return false;
    }
    clear_stall_watchdog_liveness_state(provider, channel_id, snapshot.tmux_session.as_deref());
    true
}

pub(super) fn stalled_undelivered_backlog_for_redrive(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    now_unix_secs: i64,
) -> bool {
    if !live_undelivered_backlog(snapshot) {
        return false;
    }

    let key = StallLivenessKey::from_snapshot(provider, channel_id, snapshot);
    let relay_observation = observe_relay_offset(&key, snapshot.last_relay_offset, now_unix_secs);
    !relay_offset_advanced_this_tick(snapshot, &relay_observation)
        && relay_offset_unchanged_past_backlog_grace(&relay_observation)
}

pub(super) fn gc_stall_watchdog_liveness_state(now_unix_secs: i64) {
    OFFSET_OBSERVATIONS.retain(|_, observation| {
        !liveness_state_expired(observation.last_updated_unix_secs, now_unix_secs)
    });
}

fn stall_watchdog_liveness_state_is_healthy(snapshot: &WatcherStateSnapshot) -> bool {
    !snapshot.inflight_state_present || snapshot.inflight_terminal_delivery_committed
}

fn liveness_state_expired(last_updated_unix_secs: i64, now_unix_secs: i64) -> bool {
    saturating_age_secs(last_updated_unix_secs, now_unix_secs) > STALL_LIVENESS_STATE_TTL_SECS
}

/// #3169: runtime jsonl / generation mtime liveness probe retained for the
/// idle-foreground watchdog branch. Returns `true` to defer cleanup this pass.
pub(super) fn stall_watchdog_jsonl_liveness_defers_force_clean(
    latest_runtime_activity_unix_nanos: i64,
    now_unix_secs: i64,
    freshness_threshold_secs: u64,
) -> bool {
    unix_nanos_age_secs(latest_runtime_activity_unix_nanos, now_unix_secs)
        .is_some_and(|age_secs| age_secs < freshness_threshold_secs)
}

pub(super) fn log_stall_watchdog_liveness_deferred(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    basis: &StallWatchdogJudgmentBasis,
    decision: &StallWatchdogLivenessDecision,
    freshness_secs: u64,
    threshold_secs: u64,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        event = "stall_watchdog_force_cleanup_deferred",
        reason_code = "1446_stall_watchdog",
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session = ?snapshot.tmux_session,
        attached = snapshot.attached,
        desynced = snapshot.desynced,
        inflight_state_present = snapshot.inflight_state_present,
        inflight_terminal_delivery_committed = snapshot.inflight_terminal_delivery_committed,
        inflight_started_at = ?snapshot.inflight_started_at,
        inflight_updated_at = ?snapshot.inflight_updated_at,
        inflight_age_secs = ?basis.inflight_age_secs,
        inflight_age_anchor_unix_secs = ?basis.inflight_age_anchor_unix_secs,
        threshold_secs = threshold_secs,
        last_relay_ts_ms = snapshot.last_relay_ts_ms,
        last_relay_age_secs = ?basis.last_relay_age_secs,
        last_relay_offset = snapshot.last_relay_offset,
        last_capture_offset = ?snapshot.last_capture_offset,
        unread_bytes = ?snapshot.unread_bytes,
        tmux_session_alive = ?snapshot.tmux_session_alive,
        watcher_owner_channel_id = ?snapshot.watcher_owner_channel_id,
        relay_stall_state = snapshot.relay_stall_state.as_str(),
        relay_active_turn = ?snapshot.relay_health.active_turn,
        mailbox_active_user_msg_id = ?snapshot.mailbox_active_user_msg_id,
        mailbox_has_cancel_token = snapshot.relay_health.mailbox_has_cancel_token,
        queue_depth = snapshot.relay_health.queue_depth,
        last_outbound_activity_age_secs = ?basis.last_outbound_activity_age_secs,
        liveness_freshness_secs = freshness_secs,
        liveness_reasons = decision.evidence.reason_codes_csv(freshness_secs),
        pane_offset_current = ?decision.evidence.pane_offset_current,
        pane_offset_previous = ?decision.evidence.pane_offset_previous,
        pane_offset_advanced_age_secs = ?decision.evidence.pane_offset_advanced_age_secs,
        relay_offset_current = ?decision.evidence.relay_offset_current,
        relay_offset_previous = ?decision.evidence.relay_offset_previous,
        relay_offset_advanced_age_secs = ?decision.evidence.relay_offset_advanced_age_secs,
        transcript_mtime_age_secs = ?decision.evidence.transcript_mtime_age_secs,
        runtime_activity_age_secs = ?decision.evidence.runtime_activity_age_secs,
        outbound_activity_age_secs = ?decision.evidence.outbound_activity_age_secs,
        background_synthetic_activity_age_secs = ?decision.evidence.background_synthetic_activity_age_secs,
        background_synthetic_kind = ?decision.evidence.background_synthetic_kind,
        has_undelivered_backlog = decision.evidence.has_undelivered_backlog,
        deferral_count = ?decision.deferral_count(),
        max_deferrals = decision.max_deferrals,
        "  [{ts}] 🌱 STALL-WATCHDOG: deferred forced cleanup for desynced channel {} (provider={}) due to positive liveness evidence",
        channel_id,
        provider.as_str(),
    );
}

pub(super) fn log_stall_watchdog_force_cleanup_judgment(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    basis: &StallWatchdogJudgmentBasis,
    decision: Option<&StallWatchdogLivenessDecision>,
    freshness_secs: u64,
    threshold_secs: u64,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    let no_evidence = decision.is_some_and(|decision| {
        matches!(
            &decision.action,
            StallWatchdogLivenessAction::ProceedNoEvidence
        )
    });
    let absolute_backstop_reached =
        decision.is_some_and(StallWatchdogLivenessDecision::absolute_backstop_reached);
    let liveness_reasons = decision
        .map(|decision| decision.evidence.reason_codes_csv(freshness_secs))
        .unwrap_or_else(|| "not_evaluated".to_string());
    tracing::warn!(
        event = "stall_watchdog_force_cleanup_judgment",
        reason_code = "1446_stall_watchdog",
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session = ?snapshot.tmux_session,
        attached = snapshot.attached,
        desynced = snapshot.desynced,
        inflight_state_present = snapshot.inflight_state_present,
        inflight_terminal_delivery_committed = snapshot.inflight_terminal_delivery_committed,
        inflight_started_at = ?snapshot.inflight_started_at,
        inflight_updated_at = ?snapshot.inflight_updated_at,
        inflight_age_secs = ?basis.inflight_age_secs,
        inflight_age_anchor_unix_secs = ?basis.inflight_age_anchor_unix_secs,
        threshold_secs = threshold_secs,
        last_relay_ts_ms = snapshot.last_relay_ts_ms,
        last_relay_age_secs = ?basis.last_relay_age_secs,
        last_relay_offset = snapshot.last_relay_offset,
        last_capture_offset = ?snapshot.last_capture_offset,
        unread_bytes = ?snapshot.unread_bytes,
        tmux_session_alive = ?snapshot.tmux_session_alive,
        watcher_owner_channel_id = ?snapshot.watcher_owner_channel_id,
        relay_stall_state = snapshot.relay_stall_state.as_str(),
        relay_active_turn = ?snapshot.relay_health.active_turn,
        mailbox_active_user_msg_id = ?snapshot.mailbox_active_user_msg_id,
        mailbox_has_cancel_token = snapshot.relay_health.mailbox_has_cancel_token,
        queue_depth = snapshot.relay_health.queue_depth,
        last_outbound_activity_age_secs = ?basis.last_outbound_activity_age_secs,
        liveness_freshness_secs = freshness_secs,
        liveness_reasons = liveness_reasons,
        liveness_no_evidence = no_evidence,
        liveness_absolute_backstop_reached = absolute_backstop_reached,
        outbound_activity_age_secs = ?decision.map(|decision| decision.evidence.outbound_activity_age_secs),
        relay_offset_advanced_age_secs = ?decision.and_then(|decision| decision.evidence.relay_offset_advanced_age_secs),
        has_undelivered_backlog = decision.is_some_and(|decision| decision.evidence.has_undelivered_backlog),
        deferral_count = ?decision.and_then(StallWatchdogLivenessDecision::deferral_count),
        max_deferrals = decision.map(|decision| decision.max_deferrals).unwrap_or(0),
        "  [{ts}] ⚡ STALL-WATCHDOG: forced cleanup for desynced channel {}",
        channel_id,
    );
}

impl StallLivenessKey {
    fn new(
        provider: &ProviderKind,
        channel_id: ChannelId,
        tmux_session: Option<&str>,
        user_msg_id: Option<u64>,
        started_at: Option<&str>,
    ) -> Self {
        Self {
            offset_kind: OffsetObservationKind::PaneCapture,
            provider: provider.as_str().to_string(),
            channel_id: channel_id.get(),
            tmux_session: tmux_session.map(str::to_string),
            user_msg_id,
            started_at: started_at.map(str::to_string),
        }
    }

    fn from_snapshot(
        provider: &ProviderKind,
        channel_id: ChannelId,
        snapshot: &WatcherStateSnapshot,
    ) -> Self {
        Self::new(
            provider,
            channel_id,
            snapshot.tmux_session.as_deref(),
            snapshot.inflight_user_msg_id,
            snapshot.inflight_started_at.as_deref(),
        )
    }

    fn for_offset_kind(&self, offset_kind: OffsetObservationKind) -> Self {
        let mut key = self.clone();
        key.offset_kind = offset_kind;
        key
    }

    fn matches_session(&self, probe: &Self) -> bool {
        self.provider == probe.provider
            && self.channel_id == probe.channel_id
            && self.tmux_session == probe.tmux_session
    }
}

impl StallWatchdogLivenessEvidence {
    fn collect(
        key: &StallLivenessKey,
        snapshot: &WatcherStateSnapshot,
        inflight: Option<&InflightTurnState>,
        now_unix_secs: i64,
    ) -> Self {
        let pane_observation =
            observe_pane_offset(key, snapshot.last_capture_offset, now_unix_secs);
        let relay_observation =
            observe_relay_offset(key, snapshot.last_relay_offset, now_unix_secs);
        let background_synthetic =
            background_synthetic_activity_age_secs(snapshot, inflight, now_unix_secs);
        Self {
            pane_offset_current: snapshot.last_capture_offset,
            pane_offset_previous: pane_observation.previous_offset,
            pane_offset_advanced_age_secs: pane_observation.advanced_age_secs,
            relay_offset_current: Some(snapshot.last_relay_offset),
            relay_offset_previous: relay_observation.previous_offset,
            relay_offset_advanced_age_secs: relay_observation.advanced_age_secs,
            transcript_mtime_age_secs: transcript_mtime_age_secs(inflight, now_unix_secs),
            runtime_activity_age_secs: runtime_activity_age_secs(snapshot, now_unix_secs),
            outbound_activity_age_secs: unix_millis_age_secs(
                snapshot.relay_health.last_outbound_activity_ms,
                now_unix_secs,
            ),
            background_synthetic_activity_age_secs: background_synthetic
                .as_ref()
                .map(|(_, age)| *age),
            background_synthetic_kind: background_synthetic.map(|(kind, _)| kind),
            has_undelivered_backlog: has_undelivered_backlog(snapshot, &relay_observation),
        }
    }
}

fn observe_pane_offset(
    key: &StallLivenessKey,
    current_offset: Option<u64>,
    now_unix_secs: i64,
) -> OffsetObservationResult {
    let key = key.for_offset_kind(OffsetObservationKind::PaneCapture);
    observe_offset(&key, current_offset, now_unix_secs)
}

fn observe_relay_offset(
    key: &StallLivenessKey,
    current_offset: u64,
    now_unix_secs: i64,
) -> OffsetObservationResult {
    let key = key.for_offset_kind(OffsetObservationKind::RelayDelivered);
    observe_offset(&key, Some(current_offset), now_unix_secs)
}

fn observe_offset(
    key: &StallLivenessKey,
    current_offset: Option<u64>,
    now_unix_secs: i64,
) -> OffsetObservationResult {
    let Some(current_offset) = current_offset else {
        OFFSET_OBSERVATIONS.remove(key);
        return OffsetObservationResult::default();
    };
    let previous = OFFSET_OBSERVATIONS.get(key).map(|entry| entry.clone());
    let advanced_at_unix_secs = match previous.as_ref() {
        Some(prev) if current_offset > prev.offset => Some(now_unix_secs),
        Some(prev) if current_offset == prev.offset => prev.advanced_at_unix_secs,
        _ => None,
    };
    let unchanged_since_unix_secs = match previous.as_ref() {
        Some(prev) if current_offset == prev.offset => prev.unchanged_since_unix_secs,
        _ => now_unix_secs,
    };
    OFFSET_OBSERVATIONS.insert(
        key.clone(),
        OffsetObservation {
            offset: current_offset,
            advanced_at_unix_secs,
            unchanged_since_unix_secs,
            last_updated_unix_secs: now_unix_secs,
        },
    );
    OffsetObservationResult {
        previous_offset: previous.map(|prev| prev.offset),
        advanced_age_secs: advanced_at_unix_secs.map(|at| saturating_age_secs(at, now_unix_secs)),
        unchanged_age_secs: Some(saturating_age_secs(
            unchanged_since_unix_secs,
            now_unix_secs,
        )),
    }
}

fn has_undelivered_backlog(
    snapshot: &WatcherStateSnapshot,
    relay_observation: &OffsetObservationResult,
) -> bool {
    if !live_undelivered_backlog(snapshot) {
        return false;
    }

    relay_offset_advanced_this_tick(snapshot, relay_observation)
        || relay_offset_unchanged_inside_backlog_grace(relay_observation)
}

fn live_undelivered_backlog(snapshot: &WatcherStateSnapshot) -> bool {
    snapshot.unread_bytes.is_some_and(|bytes| bytes > 0)
        && snapshot.tmux_session_alive == Some(true)
        && !snapshot.inflight_terminal_delivery_committed
}

fn relay_offset_advanced_this_tick(
    snapshot: &WatcherStateSnapshot,
    relay_observation: &OffsetObservationResult,
) -> bool {
    relay_observation
        .previous_offset
        .is_some_and(|previous| snapshot.last_relay_offset > previous)
}

fn relay_offset_unchanged_inside_backlog_grace(
    relay_observation: &OffsetObservationResult,
) -> bool {
    relay_observation
        .unchanged_age_secs
        .is_some_and(|age| age < STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS)
}

fn relay_offset_unchanged_past_backlog_grace(relay_observation: &OffsetObservationResult) -> bool {
    relay_observation
        .unchanged_age_secs
        .is_some_and(|age| age >= STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS)
}

fn transcript_mtime_age_secs(
    inflight: Option<&InflightTurnState>,
    now_unix_secs: i64,
) -> Option<u64> {
    let path = inflight
        .and_then(|state| state.output_path.as_deref())
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    path_mtime_unix_nanos(path).and_then(|nanos| unix_nanos_age_secs(nanos, now_unix_secs))
}

fn runtime_activity_age_secs(snapshot: &WatcherStateSnapshot, now_unix_secs: i64) -> Option<u64> {
    let tmux_session = snapshot.tmux_session.as_deref()?;
    let nanos =
        crate::services::dispatched_sessions::latest_runtime_activity_unix_nanos(tmux_session);
    unix_nanos_age_secs(nanos, now_unix_secs)
}

fn background_synthetic_activity_age_secs(
    snapshot: &WatcherStateSnapshot,
    inflight: Option<&InflightTurnState>,
    now_unix_secs: i64,
) -> Option<(String, u64)> {
    let (kind, updated_at_ms) = background_synthetic_activity_anchor(snapshot, inflight)?;
    unix_millis_age_secs(Some(updated_at_ms), now_unix_secs).map(|age| (kind, age))
}

fn background_synthetic_activity_anchor(
    snapshot: &WatcherStateSnapshot,
    inflight: Option<&InflightTurnState>,
) -> Option<(String, i64)> {
    let inflight_updated_ms = inflight
        .and_then(|state| {
            crate::services::discord::inflight::parse_updated_at_unix(&state.updated_at)
        })
        .and_then(|seconds| seconds.checked_mul(1000));
    let activity_ms = max_optional_i64([
        snapshot.relay_health.last_outbound_activity_ms,
        positive_millis(snapshot.last_relay_ts_ms),
        inflight_updated_ms,
    ])?;

    if snapshot.relay_health.active_turn == RelayActiveTurn::ExplicitBackground {
        return Some((
            "relay_active_turn:explicit_background".to_string(),
            activity_ms,
        ));
    }
    if let Some(kind) = inflight.and_then(|state| state.task_notification_kind) {
        return Some((
            format!("task_notification_kind:{}", task_kind_str(kind)),
            activity_ms,
        ));
    }
    if inflight.is_some_and(|state| state.long_running_placeholder_active) {
        return Some(("long_running_placeholder_active".to_string(), activity_ms));
    }
    if inflight.is_some_and(|state| {
        state.rebind_origin || state.user_msg_id == 0 || state.current_msg_id == 0
    }) {
        return Some(("synthetic_turn".to_string(), activity_ms));
    }
    None
}

fn task_kind_str(kind: TaskNotificationKind) -> &'static str {
    kind.as_str()
}

fn max_optional_i64<const N: usize>(values: [Option<i64>; N]) -> Option<i64> {
    values.into_iter().flatten().max()
}

fn path_mtime_unix_nanos(path: &str) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
}

fn positive_millis(value: i64) -> Option<i64> {
    (value > 0).then_some(value)
}

fn unix_millis_age_secs(unix_millis: Option<i64>, now_unix_secs: i64) -> Option<u64> {
    let millis = unix_millis?;
    let now_millis = now_unix_secs.saturating_mul(1000);
    if millis >= now_millis {
        return Some(0);
    }
    Some((now_millis.saturating_sub(millis) as u64) / 1000)
}

fn unix_nanos_age_secs(unix_nanos: i64, now_unix_secs: i64) -> Option<u64> {
    if unix_nanos <= 0 {
        return None;
    }
    let now_nanos = now_unix_secs.saturating_mul(1_000_000_000);
    if unix_nanos >= now_nanos {
        return Some(0);
    }
    Some((now_nanos.saturating_sub(unix_nanos) as u64) / 1_000_000_000)
}

fn saturating_age_secs(anchor_unix_secs: i64, now_unix_secs: i64) -> u64 {
    now_unix_secs.saturating_sub(anchor_unix_secs).max(0) as u64
}

fn is_recent_age(age_secs: Option<u64>, freshness_secs: u64) -> bool {
    age_secs.is_some_and(|age| age < freshness_secs)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use chrono::TimeZone;
    use poise::serenity_prelude::ChannelId;
    use tracing_subscriber::fmt::MakeWriter;

    use crate::services::discord::relay_health::{RelayHealthSnapshot, RelayStallState};

    use super::*;

    /// Restores an env var to its prior value on drop (mirrors the helper in the
    /// sibling `recovery.rs` test module).
    struct EnvVarReset {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarReset {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarReset {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    /// The liveness evidence path reaches `latest_runtime_activity_unix_nanos`,
    /// which trips the #3293 runtime-store guard when `AGENTDESK_ROOT_DIR` points
    /// at a live release root (the normal dev-machine env). Point it at a throw-
    /// away temp root so these tests run anywhere, not just under CI's temp root.
    ///
    /// `AGENTDESK_ROOT_DIR` is process-global, so we hold the shared test env lock
    /// (same as the sibling `recovery.rs` tests) for the whole test — otherwise a
    /// parallel test's `Drop` could restore the live root mid-run. Hold the
    /// returned tuple for the whole test. Tuple drop order is first-to-last, so it
    /// restores the env, deletes the temp dir, then releases the lock — all while
    /// still holding the lock.
    #[must_use]
    fn isolated_runtime_root() -> (
        EnvVarReset,
        tempfile::TempDir,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().expect("temp runtime root");
        let env = EnvVarReset::set("AGENTDESK_ROOT_DIR", dir.path());
        (env, dir, lock)
    }

    #[derive(Clone)]
    struct CapturingWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_warns<F>(f: F) -> String
    where
        F: FnOnce(),
    {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = CapturingWriter {
            buffer: buffer.clone(),
        };
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(writer)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        f();
        drop(guard);
        String::from_utf8_lossy(&buffer.lock().unwrap().clone()).into_owned()
    }

    fn snapshot(
        channel_id: u64,
        tmux_session: &str,
        capture_offset: Option<u64>,
    ) -> WatcherStateSnapshot {
        WatcherStateSnapshot {
            provider: ProviderKind::Codex.as_str().to_string(),
            attached: true,
            tmux_session: Some(tmux_session.to_string()),
            watcher_owner_channel_id: Some(channel_id),
            last_relay_offset: 10,
            inflight_state_present: true,
            last_relay_ts_ms: 1_700_000_000_000,
            last_capture_offset: capture_offset,
            unread_bytes: capture_offset.map(|offset| offset.saturating_sub(10)),
            desynced: true,
            reconnect_count: 0,
            inflight_started_at: Some("2026-06-12 00:00:00".to_string()),
            inflight_updated_at: Some("2026-06-12 00:00:00".to_string()),
            inflight_user_msg_id: Some(9001),
            inflight_current_msg_id: Some(9002),
            tmux_session_alive: Some(true),
            has_pending_queue: false,
            mailbox_active_user_msg_id: Some(9001),
            inflight_terminal_delivery_committed: false,
            inflight_identity: None,
            inflight_finalizer_turn_id: None,
            inflight_output_path: Some(format!("/tmp/{tmux_session}.jsonl")),
            relay_stall_state: RelayStallState::TmuxAliveRelayDead,
            relay_health: RelayHealthSnapshot {
                provider: ProviderKind::Codex.as_str().to_string(),
                channel_id,
                active_turn: RelayActiveTurn::Foreground,
                tmux_session: Some(tmux_session.to_string()),
                tmux_alive: Some(true),
                watcher_attached: true,
                watcher_attached_stale: false,
                watcher_owner_channel_id: Some(channel_id),
                watcher_owns_live_relay: true,
                bridge_inflight_present: true,
                bridge_current_msg_id: Some(9002),
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: Some(9001),
                queue_depth: 0,
                pending_discord_callback_msg_id: Some(9002),
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: Some(1_700_000_000_000),
                last_outbound_activity_ms: None,
                last_capture_offset: capture_offset,
                last_relay_offset: 10,
                unread_bytes: capture_offset.map(|offset| offset.saturating_sub(10)),
                desynced: true,
                stale_thread_proof: false,
            },
        }
    }

    fn inflight_with_output(
        channel_id: u64,
        tmux_session: &str,
        path: Option<String>,
    ) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            None,
            1,
            9001,
            9002,
            "test".to_string(),
            Some("session".to_string()),
            Some(tmux_session.to_string()),
            path,
            None,
            0,
        )
    }

    fn liveness_key(
        provider: &ProviderKind,
        channel: ChannelId,
        tmux_session: &str,
    ) -> StallLivenessKey {
        StallLivenessKey::new(
            provider,
            channel,
            Some(tmux_session),
            Some(9001),
            Some("2026-06-12 00:00:00"),
        )
    }

    fn liveness_state_present(key: &StallLivenessKey) -> bool {
        OFFSET_OBSERVATIONS.contains_key(key)
    }

    #[test]
    fn positive_liveness_defers_cleanup_and_logs_reason() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3361);
        let tmux_session = "AgentDesk-codex-liveness-defers";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let file = tempfile::NamedTempFile::new().expect("temp transcript");
        let inflight = inflight_with_output(
            channel.get(),
            tmux_session,
            Some(file.path().display().to_string()),
        );
        let mut snap = snapshot(channel.get(), tmux_session, Some(20));
        snap.unread_bytes = Some(0);
        snap.relay_health.unread_bytes = Some(0);
        let now = chrono::Utc::now().timestamp();

        let decision = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(0),
        );
        assert!(decision.should_defer());
        assert_eq!(
            decision
                .evidence
                .reason_codes_csv(STALL_WATCHDOG_POSITIVE_LIVENESS_SECS),
            "transcript_mtime_recent"
        );

        let basis = StallWatchdogJudgmentBasis::from_snapshot(&snap, now, now - 10_000);
        let logs = capture_warns(|| {
            log_stall_watchdog_liveness_deferred(
                &provider,
                channel,
                &snap,
                &basis,
                &decision,
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                600,
            );
        });
        assert!(
            logs.contains("stall_watchdog_force_cleanup_deferred"),
            "{logs}"
        );
        assert!(logs.contains("transcript_mtime_recent"), "{logs}");
    }

    #[test]
    fn no_liveness_evidence_proceeds_with_existing_cleanup() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3362);
        let tmux_session = "AgentDesk-codex-no-liveness";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let snap = snapshot(channel.get(), tmux_session, None);
        let mut inflight = inflight_with_output(channel.get(), tmux_session, None);
        inflight.updated_at = "2026-06-12 00:00:00".to_string();

        let decision = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            1_800_000_000,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(0),
        );
        assert_eq!(
            decision.action,
            StallWatchdogLivenessAction::ProceedNoEvidence
        );
        assert!(!decision.should_defer());
    }

    #[test]
    fn advancing_relay_offset_defers_cleanup() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4178_001);
        let tmux_session = "AgentDesk-codex-relay-offset-liveness";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let mut snap = snapshot(channel.get(), tmux_session, None);
        snap.unread_bytes = None;
        snap.relay_health.unread_bytes = None;
        let mut inflight = inflight_with_output(channel.get(), tmux_session, None);
        inflight.updated_at = "2026-06-12 00:00:00".to_string();

        let first = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            1_800_000_000,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(0),
        );
        assert_eq!(first.action, StallWatchdogLivenessAction::ProceedNoEvidence);

        snap.last_relay_offset = 64;
        snap.relay_health.last_relay_offset = 64;
        let advanced = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            1_800_000_005,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(0),
        );

        assert_eq!(
            advanced.action,
            StallWatchdogLivenessAction::Defer { deferral_count: 0 }
        );
        assert_eq!(advanced.evidence.relay_offset_previous, Some(10));
        assert_eq!(advanced.evidence.relay_offset_current, Some(64));
        assert_eq!(advanced.evidence.relay_offset_advanced_age_secs, Some(0));
        assert_eq!(
            advanced
                .evidence
                .reason_codes_csv(STALL_WATCHDOG_POSITIVE_LIVENESS_SECS),
            "relay_offset_advanced_recently"
        );

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    #[test]
    fn frozen_undelivered_backlog_cleans_after_no_progress_grace() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4178_002);
        let tmux_session = "AgentDesk-codex-frozen-backlog-liveness";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let mut snap = snapshot(channel.get(), tmux_session, None);
        snap.unread_bytes = Some(301_603);
        snap.relay_health.unread_bytes = Some(301_603);
        snap.tmux_session_alive = Some(true);
        snap.inflight_terminal_delivery_committed = false;
        let mut inflight = inflight_with_output(channel.get(), tmux_session, None);
        inflight.updated_at = "2026-06-12 00:00:00".to_string();
        let now = 1_800_000_000;

        let first = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(0),
        );
        assert_eq!(
            first.action,
            StallWatchdogLivenessAction::Defer { deferral_count: 0 },
            "first backlog observation gets only the short no-progress grace"
        );
        assert!(first.evidence.has_undelivered_backlog);
        assert_eq!(
            first
                .evidence
                .reason_codes_csv(STALL_WATCHDOG_POSITIVE_LIVENESS_SECS),
            "has_undelivered_backlog"
        );

        let still_inside_grace = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now + STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64 - 1,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(0),
        );
        assert_eq!(
            still_inside_grace.action,
            StallWatchdogLivenessAction::Defer { deferral_count: 0 },
            "a frozen backlog may defer only inside the bounded grace"
        );

        let expired = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now + STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(0),
        );
        assert_eq!(
            expired.action,
            StallWatchdogLivenessAction::ProceedNoEvidence,
            "a frozen backlog must clean when the no-progress grace expires"
        );
        assert!(!expired.evidence.has_undelivered_backlog);
        assert_eq!(
            expired
                .evidence
                .reason_codes_csv(STALL_WATCHDOG_POSITIVE_LIVENESS_SECS),
            "none"
        );

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    #[test]
    fn draining_undelivered_backlog_keeps_deferring_across_ticks_until_drained() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4178_003);
        let tmux_session = "AgentDesk-codex-draining-backlog-liveness";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let capture_offset = 301_613;
        let mut snap = snapshot(channel.get(), tmux_session, Some(capture_offset));
        snap.tmux_session_alive = Some(true);
        snap.inflight_terminal_delivery_committed = false;
        let mut inflight = inflight_with_output(channel.get(), tmux_session, None);
        inflight.updated_at = "2026-06-12 00:00:00".to_string();
        let now = 1_800_000_000;

        let first = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(0),
        );
        assert!(first.should_defer());
        assert!(first.evidence.has_undelivered_backlog);

        for (tick, delivered_offset) in [(1, 64), (2, 128), (3, 192)] {
            snap.last_relay_offset = delivered_offset;
            snap.relay_health.last_relay_offset = delivered_offset;
            let unread = capture_offset.saturating_sub(delivered_offset);
            snap.unread_bytes = Some(unread);
            snap.relay_health.unread_bytes = Some(unread);

            let decision = evaluate_stall_watchdog_liveness(
                &provider,
                channel,
                &snap,
                Some(&inflight),
                now + i64::from(tick) * 30,
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
                Some(0),
            );

            assert_eq!(
                decision.action,
                StallWatchdogLivenessAction::Defer { deferral_count: 0 },
                "tick {tick} with a shrinking backlog must keep deferring"
            );
            assert!(decision.evidence.has_undelivered_backlog);
            assert_eq!(
                decision.evidence.relay_offset_current,
                Some(delivered_offset)
            );
            assert_eq!(decision.evidence.relay_offset_advanced_age_secs, Some(0));
            assert!(
                decision
                    .evidence
                    .reason_codes_csv(STALL_WATCHDOG_POSITIVE_LIVENESS_SECS)
                    .contains("has_undelivered_backlog")
            );
        }

        snap.last_relay_offset = capture_offset;
        snap.relay_health.last_relay_offset = capture_offset;
        snap.unread_bytes = Some(0);
        snap.relay_health.unread_bytes = Some(0);
        let drained = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now + 120,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(0),
        );
        assert!(!drained.evidence.has_undelivered_backlog);

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    #[test]
    fn recent_outbound_activity_defers_cleanup() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3373);
        let tmux_session = "AgentDesk-codex-outbound-liveness";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let now = chrono::Utc::now().timestamp();
        let mut snap = snapshot(channel.get(), tmux_session, None);
        snap.relay_health.last_outbound_activity_ms = Some((now - 60) * 1000);
        let mut inflight = inflight_with_output(channel.get(), tmux_session, None);
        inflight.updated_at = "2026-06-12 00:00:00".to_string();

        let decision = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(0),
        );

        assert_eq!(
            decision.action,
            StallWatchdogLivenessAction::Defer { deferral_count: 0 }
        );
        assert_eq!(
            decision
                .evidence
                .reason_codes_csv(STALL_WATCHDOG_POSITIVE_LIVENESS_SECS),
            "outbound_activity_recent"
        );
    }

    #[test]
    fn judgment_basis_uses_started_at_not_updated_at() {
        let channel = ChannelId::new(3370);
        let tmux_session = "AgentDesk-codex-liveness-started-anchor";
        let mut snap = snapshot(channel.get(), tmux_session, None);
        snap.inflight_started_at = Some("2026-06-12 00:10:00".to_string());
        snap.inflight_updated_at = Some("2026-06-12 00:00:00".to_string());

        let now = chrono::Local
            .with_ymd_and_hms(2026, 6, 12, 0, 10, 5)
            .single()
            .expect("valid local time")
            .timestamp();
        let basis = StallWatchdogJudgmentBasis::from_snapshot(&snap, now, now - 10_000);

        assert_eq!(
            basis.inflight_age_secs,
            Some(5),
            "watchdog liveness logs/judgment must age the current turn from started_at"
        );
    }

    /// #3671: positive liveness defers indefinitely up to the *age*-based
    /// absolute backstop — it is no longer the tick count that triggers cleanup.
    /// We first prove that far more than the old 20-tick cap of deferrals all
    /// stay `Defer` while the turn's age is below the backstop, then that a turn
    /// whose age has crossed `STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS` (a genuine
    /// forever-spinner, #3582 R1 finite ceiling) force-cleans and logs the
    /// reason. [acceptance 3]
    #[test]
    fn liveness_force_clean_after_absolute_backstop_and_logs_reason() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3363);
        let tmux_session = "AgentDesk-codex-liveness-cap";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let file = tempfile::NamedTempFile::new().expect("temp transcript");
        let inflight = inflight_with_output(
            channel.get(),
            tmux_session,
            Some(file.path().display().to_string()),
        );
        let snap = snapshot(channel.get(), tmux_session, Some(20));
        let now = chrono::Utc::now().timestamp();

        // Age below the backstop: every tick well past the old cap stays a Defer,
        // but positive liveness does not consume the forced-clean escalation budget.
        let below_backstop = STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS - 1;
        for pass in 1..=(STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS + 5) {
            let decision = evaluate_stall_watchdog_liveness(
                &provider,
                channel,
                &snap,
                Some(&inflight),
                now,
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
                Some(below_backstop),
            );
            assert_eq!(
                decision.action,
                StallWatchdogLivenessAction::Defer { deferral_count: 0 },
                "pass {pass} below the absolute backstop must defer without consuming budget"
            );
        }

        // Age at/over the backstop: the forever-spinner is force-cleaned.
        let over_backstop = STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS + 3600;
        let decision = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(over_backstop),
        );
        assert_eq!(
            decision.action,
            StallWatchdogLivenessAction::ProceedAfterAbsoluteBackstop {
                age_secs: over_backstop,
                deferral_count: 0,
            }
        );

        let basis = StallWatchdogJudgmentBasis::from_snapshot(&snap, now, now - 10_000);
        let logs = capture_warns(|| {
            log_stall_watchdog_force_cleanup_judgment(
                &provider,
                channel,
                &snap,
                &basis,
                Some(&decision),
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                600,
            );
        });
        assert!(
            logs.contains("stall_watchdog_force_cleanup_judgment"),
            "{logs}"
        );
        assert!(
            logs.contains("liveness_absolute_backstop_reached=true"),
            "{logs}"
        );
    }

    #[test]
    fn positive_liveness_does_not_preserve_deferral_budget_across_turns() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3371);
        let tmux_session = "AgentDesk-codex-liveness-turn-identity";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let file = tempfile::NamedTempFile::new().expect("temp transcript");
        let inflight = inflight_with_output(
            channel.get(),
            tmux_session,
            Some(file.path().display().to_string()),
        );
        let snap = snapshot(channel.get(), tmux_session, Some(20));
        let now = chrono::Utc::now().timestamp();

        for pass in 1..=2 {
            let decision = evaluate_stall_watchdog_liveness(
                &provider,
                channel,
                &snap,
                Some(&inflight),
                now,
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
                Some(0),
            );
            assert_eq!(
                decision.action,
                StallWatchdogLivenessAction::Defer { deferral_count: 0 },
                "positive liveness pass {pass} must not consume budget"
            );
        }

        let mut next_turn = snap.clone();
        next_turn.inflight_user_msg_id = Some(9003);
        next_turn.mailbox_active_user_msg_id = Some(9003);
        next_turn.inflight_started_at = Some("2026-06-12 00:05:00".to_string());
        let decision = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &next_turn,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(0),
        );

        assert_eq!(
            decision.action,
            StallWatchdogLivenessAction::Defer { deferral_count: 0 },
            "a new user_msg_id + started_at under the same tmux session still defers without preserving budget"
        );

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #3582 + #3671 regression: the 2026-06-18 12:07 false-positive AND the
    /// #3671 ~40-minute deploy turn. A live turn that keeps emitting output (fresh
    /// transcript mtime every tick) was force-cleaned the instant a tick *count*
    /// cap was hit (3 originally, then 20), even though `reason_codes` still listed
    /// positive liveness. #3671 removes the tick-count cleanup gate entirely: while
    /// the turn's age is below the absolute backstop, *every* tick — far past the
    /// old cap — stays a `Defer`, so a live-but-quiet turn survives indefinitely.
    /// [acceptance 1]
    #[test]
    fn strong_liveness_defers_indefinitely_below_absolute_backstop() {
        const OLD_CAP: u8 = 20;

        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3368);
        let tmux_session = "AgentDesk-codex-liveness-12-07";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        // A fresh temp transcript => `transcript_mtime_recent` is positive on
        // every tick, mirroring the live turn whose JSONL kept being written.
        let file = tempfile::NamedTempFile::new().expect("temp transcript");
        let inflight = inflight_with_output(
            channel.get(),
            tmux_session,
            Some(file.path().display().to_string()),
        );
        let snap = snapshot(channel.get(), tmux_session, Some(20));
        let now = chrono::Utc::now().timestamp();

        // Every tick well beyond the old cap must STILL defer. Under the old
        // tick-count cap the (OLD_CAP+1)th pass force-cleaned a live turn; under
        // the age-based backstop (age held below the ceiling) it stays a Defer.
        let below_backstop = STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS - 1;
        for pass in 1..=(OLD_CAP * 3) {
            let decision = evaluate_stall_watchdog_liveness(
                &provider,
                channel,
                &snap,
                Some(&inflight),
                now,
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
                Some(below_backstop),
            );
            assert_eq!(
                decision.action,
                StallWatchdogLivenessAction::Defer { deferral_count: 0 },
                "pass {pass} must still defer below the absolute backstop"
            );
        }

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #3671 deploy scenario, end-to-end: a ~40-minute turn that survived a
    /// mid-turn restart. `started_at` is 40 minutes in the past and `boot` = now
    /// (the restart instant). The backstop measures the RAW turn age
    /// (`turn_age_secs` = ~40m, NOT the boot-floored anchor), which is far below
    /// the 4h ceiling, so with positive liveness it must keep deferring, never
    /// force-cleaned. The boot-floored `inflight_age_secs` is ~0 here (it only
    /// governs the separate post-boot grace on the threshold gate). [acceptance 1]
    #[test]
    fn deploy_restart_40min_turn_survives() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3372);
        let tmux_session = "AgentDesk-codex-liveness-deploy-restart";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let file = tempfile::NamedTempFile::new().expect("temp transcript");
        let inflight = inflight_with_output(
            channel.get(),
            tmux_session,
            Some(file.path().display().to_string()),
        );
        let now = chrono::Utc::now().timestamp();
        let mut snap = snapshot(channel.get(), tmux_session, Some(20));
        // `parse_updated_at_unix` interprets the stamp as LOCAL time, so build it
        // in Local to round-trip to exactly `now - 2400` (see judgment_basis test).
        let started_at = chrono::Local
            .timestamp_opt(now - 2400, 0)
            .single()
            .expect("valid started_at")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        snap.inflight_started_at = Some(started_at);
        let basis = StallWatchdogJudgmentBasis::from_snapshot(&snap, now, now);
        // Boot-floored anchor (threshold-gate grace) is ~0 after the restart...
        assert!(
            basis.inflight_age_secs.is_some_and(|age| age < 60),
            "post-restart boot-floored anchor ⇒ age is ~0, got {:?}",
            basis.inflight_age_secs
        );
        // ...but the backstop sees the turn's RAW ~40-minute age, well below 4h.
        assert!(
            basis
                .turn_age_secs
                .is_some_and(|age| (2340..=2460).contains(&age)),
            "raw turn age must be ~40 minutes regardless of restart, got {:?}",
            basis.turn_age_secs
        );

        let decision = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            basis.turn_age_secs,
        );
        assert!(
            decision.should_defer(),
            "a live ~40-minute deploy turn that survived a restart must keep deferring, got {:?}",
            decision.action
        );

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #3671 regression — the codex-review finding that the "absolute" backstop
    /// must survive restarts. A genuine forever-spinner started 5h ago but the
    /// dcserver just restarted (boot = now), so the boot-floored anchor age is ~0
    /// and the OLD design (backstop on the boot-floored age) would defer forever,
    /// re-armed by every restart. The backstop now measures the RAW turn age
    /// (5h ≥ 4h ceiling), so it force-cleans even immediately after a restart —
    /// the finite detection ceiling (#3582 R1) cannot be reset by restart churn.
    #[test]
    fn forever_spinner_survives_restarts_still_bounded_by_absolute_backstop() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3373);
        let tmux_session = "AgentDesk-codex-liveness-forever-spinner-restart";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let file = tempfile::NamedTempFile::new().expect("temp transcript");
        let inflight = inflight_with_output(
            channel.get(),
            tmux_session,
            Some(file.path().display().to_string()),
        );
        let now = chrono::Utc::now().timestamp();
        let mut snap = snapshot(channel.get(), tmux_session, Some(20));
        // Turn started 5h ago; backstop ceiling is 4h. boot = now (just restarted).
        let raw_age_secs = STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS as i64 + 3600;
        // Built in Local so it round-trips through `parse_updated_at_unix`.
        let started_at = chrono::Local
            .timestamp_opt(now - raw_age_secs, 0)
            .single()
            .expect("valid started_at")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        snap.inflight_started_at = Some(started_at);
        let basis = StallWatchdogJudgmentBasis::from_snapshot(&snap, now, now);
        // The boot-floored anchor hides the true age (this is the bug surface)...
        assert!(
            basis.inflight_age_secs.is_some_and(|age| age < 60),
            "boot-floored anchor resets to ~0 on restart, got {:?}",
            basis.inflight_age_secs
        );
        // ...but the RAW turn age the backstop uses is past the ceiling.
        assert!(
            basis
                .turn_age_secs
                .is_some_and(|age| age >= STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS),
            "raw turn age must cross the backstop despite the restart, got {:?}",
            basis.turn_age_secs
        );

        let decision = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            basis.turn_age_secs,
        );
        assert!(
            matches!(
                decision.action,
                StallWatchdogLivenessAction::ProceedAfterAbsoluteBackstop { .. }
            ),
            "a 5h forever-spinner must force-clean even right after a restart, got {:?}",
            decision.action
        );

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #3582 + #3671 corollary: the deferral changes must NOT weaken detection of
    /// a genuinely dead relay. When no liveness signal is present
    /// (`reason_codes == none`), the decision is `ProceedNoEvidence` on the very
    /// first tick regardless of age or the absolute backstop — exactly the
    /// 11:52 / 12:38 immediate-clean cases. Here the age is held below the
    /// backstop to prove the no-evidence branch fires *before* any backstop
    /// consideration. [acceptance 2]
    #[test]
    fn no_liveness_still_proceeds_immediately_under_raised_cap() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3369);
        let tmux_session = "AgentDesk-codex-liveness-dead-relay";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        // No transcript path => no transcript mtime signal; stale inflight =>
        // no other positive signal either.
        let snap = snapshot(channel.get(), tmux_session, None);
        let mut inflight = inflight_with_output(channel.get(), tmux_session, None);
        inflight.updated_at = "2026-06-12 00:00:00".to_string();

        let decision = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            1_800_000_000,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS - 1),
        );
        assert_eq!(
            decision.action,
            StallWatchdogLivenessAction::ProceedNoEvidence,
            "a dead relay must be cleaned on the first tick even with a raised cap"
        );

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    #[test]
    fn positive_liveness_resets_deferral_budget_across_desync_flap() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3364);
        let tmux_session = "AgentDesk-codex-liveness-flap";
        let _root = isolated_runtime_root();
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let file = tempfile::NamedTempFile::new().expect("temp transcript");
        let inflight = inflight_with_output(
            channel.get(),
            tmux_session,
            Some(file.path().display().to_string()),
        );
        let snap = snapshot(channel.get(), tmux_session, Some(20));
        let now = chrono::Utc::now().timestamp();
        // Age held below the absolute backstop: cleanup never fires on tick count.
        let below_backstop = STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS - 1;

        // Repeated positive liveness ticks must not build an escalation streak.
        for pass in 1..STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS {
            let decision = evaluate_stall_watchdog_liveness(
                &provider,
                channel,
                &snap,
                Some(&inflight),
                now,
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
                Some(below_backstop),
            );
            assert_eq!(
                decision.action,
                StallWatchdogLivenessAction::Defer { deferral_count: 0 },
                "positive liveness pass {pass} must not consume budget"
            );
        }

        // A transient desync flap (desynced toggles off but terminal delivery
        // never committed) must not resurrect stale budget state.
        let mut flapped_snapshot = snap.clone();
        flapped_snapshot.desynced = false;
        flapped_snapshot.relay_health.desynced = false;
        assert!(!clear_stall_watchdog_liveness_state_if_healthy(
            &provider,
            channel,
            &flapped_snapshot,
        ));

        // #3671: the next ticks reach and then exceed the old tick-count cap, yet
        // because the turn's age is still below the absolute backstop they all
        // stay `Defer` — the tick count no longer triggers cleanup.
        let at_cap = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(below_backstop),
        );
        assert_eq!(
            at_cap.action,
            StallWatchdogLivenessAction::Defer { deferral_count: 0 }
        );

        let decision = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(below_backstop),
        );
        assert_eq!(
            decision.action,
            StallWatchdogLivenessAction::Defer { deferral_count: 0 },
            "past the old cap but below the absolute backstop must keep deferring without budget consumption"
        );

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    #[test]
    fn healthy_recovery_clears_all_liveness_state() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3365);
        let tmux_session = "AgentDesk-codex-liveness-healthy-clear";
        let _root = isolated_runtime_root();
        let key = liveness_key(&provider, channel, tmux_session);
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let file = tempfile::NamedTempFile::new().expect("temp transcript");
        let inflight = inflight_with_output(
            channel.get(),
            tmux_session,
            Some(file.path().display().to_string()),
        );
        let snap = snapshot(channel.get(), tmux_session, Some(20));
        let now = chrono::Utc::now().timestamp();

        let decision = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            Some(0),
        );
        assert!(decision.should_defer());
        assert!(liveness_state_present(&key));

        let mut healthy_snapshot = snap.clone();
        healthy_snapshot.inflight_terminal_delivery_committed = true;
        assert!(clear_stall_watchdog_liveness_state_if_healthy(
            &provider,
            channel,
            &healthy_snapshot,
        ));
        assert!(!liveness_state_present(&key));
    }

    #[test]
    fn ttl_gc_removes_stale_liveness_state_and_keeps_fresh_entries() {
        let provider = ProviderKind::Codex;
        let old_channel = ChannelId::new(3366);
        let fresh_channel = ChannelId::new(3367);
        let old_tmux_session = "AgentDesk-codex-liveness-ttl-old";
        let fresh_tmux_session = "AgentDesk-codex-liveness-ttl-fresh";
        let old_key = liveness_key(&provider, old_channel, old_tmux_session);
        let fresh_key = liveness_key(&provider, fresh_channel, fresh_tmux_session);
        clear_stall_watchdog_liveness_state(&provider, old_channel, Some(old_tmux_session));
        clear_stall_watchdog_liveness_state(&provider, fresh_channel, Some(fresh_tmux_session));

        let now = 10_000;
        let expired_at = now - STALL_LIVENESS_STATE_TTL_SECS as i64 - 1;
        let fresh_at = now - STALL_LIVENESS_STATE_TTL_SECS as i64;
        OFFSET_OBSERVATIONS.insert(
            old_key.clone(),
            OffsetObservation {
                offset: 20,
                advanced_at_unix_secs: Some(expired_at),
                last_updated_unix_secs: expired_at,
            },
        );
        OFFSET_OBSERVATIONS.insert(
            fresh_key.clone(),
            OffsetObservation {
                offset: 30,
                advanced_at_unix_secs: Some(fresh_at),
                last_updated_unix_secs: fresh_at,
            },
        );

        gc_stall_watchdog_liveness_state(now);

        assert!(!liveness_state_present(&old_key));
        assert!(liveness_state_present(&fresh_key));
        clear_stall_watchdog_liveness_state(&provider, fresh_channel, Some(fresh_tmux_session));
    }
}
