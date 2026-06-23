use std::sync::LazyLock;

use poise::serenity_prelude::ChannelId;

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::relay_health::RelayActiveTurn;
use crate::services::provider::ProviderKind;

use super::snapshot::WatcherStateSnapshot;

pub(super) const STALL_WATCHDOG_POSITIVE_LIVENESS_SECS: u64 = 120;
/// Hard ceiling on how many consecutive watchdog ticks a force-clean may be
/// deferred while positive liveness keeps being observed. A deferral only ever
/// fires when `has_positive_liveness` is true — i.e. fresh bytes are demonstrably
/// flowing (pane offset advanced cross-tick, transcript/runtime jsonl mtime
/// inside `POSITIVE_LIVENESS_SECS`, or a fresh background-synthetic anchor) — so
/// a genuinely dead relay (every signal stale ⇒ `reason_codes == none`) takes the
/// `ProceedNoEvidence` branch and is cleaned on the very first tick, untouched by
/// this cap.
///
/// #3582: raised 3 -> 20. At the old value a *live* turn that kept emitting output
/// for longer than `THRESHOLD_SECS + 3 * INTERVAL_SECS` (~600s + ~90s) was killed
/// mid-stream the instant the cap was hit even though `reason_codes` still listed
/// `pane_offset_advanced_recently,transcript_mtime_recent` — the confirmed
/// 2026-06-18 12:07 false-positive (a "Response sent" landed 5s after the
/// force-clean). The window is only ~90s of grace over the threshold, far short of
/// a long but live turn. 20 deferrals = `20 * INTERVAL_SECS` (~600s) of extra
/// grace, so the watchdog still only kills a turn that has been quiet (no
/// liveness signal) — and a true live-spinner hang (pane bytes flow but no answer
/// ever lands) is still bounded: once the cap is reached the force-clean proceeds,
/// so the detection ceiling stays finite (R1).
pub(super) const STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS: u8 = 20;
pub(super) const STALL_LIVENESS_STATE_TTL_SECS: u64 = 1800;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct StallLivenessKey {
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
    last_updated_unix_secs: i64,
}

#[derive(Clone, Debug)]
struct DeferralState {
    count: u8,
    last_updated_unix_secs: i64,
}

static OFFSET_OBSERVATIONS: LazyLock<dashmap::DashMap<StallLivenessKey, OffsetObservation>> =
    LazyLock::new(dashmap::DashMap::new);
static DEFERRAL_STATE: LazyLock<dashmap::DashMap<StallLivenessKey, DeferralState>> =
    LazyLock::new(dashmap::DashMap::new);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StallWatchdogLivenessAction {
    ProceedNoEvidence,
    Defer { deferral_count: u8 },
    ProceedAfterDeferralLimit { previous_deferrals: u8 },
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
            StallWatchdogLivenessAction::ProceedAfterDeferralLimit { previous_deferrals } => {
                Some(previous_deferrals)
            }
            StallWatchdogLivenessAction::ProceedNoEvidence => None,
        }
    }

    fn limit_reached(&self) -> bool {
        matches!(
            self.action,
            StallWatchdogLivenessAction::ProceedAfterDeferralLimit { .. }
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct StallWatchdogLivenessEvidence {
    pub(super) pane_offset_current: Option<u64>,
    pub(super) pane_offset_previous: Option<u64>,
    pub(super) pane_offset_advanced_age_secs: Option<u64>,
    pub(super) transcript_mtime_age_secs: Option<u64>,
    pub(super) runtime_activity_age_secs: Option<u64>,
    pub(super) background_synthetic_activity_age_secs: Option<u64>,
    pub(super) background_synthetic_kind: Option<String>,
}

impl StallWatchdogLivenessEvidence {
    fn reason_codes(&self, freshness_secs: u64) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if is_recent_age(self.pane_offset_advanced_age_secs, freshness_secs) {
            reasons.push("pane_offset_advanced_recently");
        }
        if is_recent_age(self.transcript_mtime_age_secs, freshness_secs) {
            reasons.push("transcript_mtime_recent");
        }
        if is_recent_age(self.runtime_activity_age_secs, freshness_secs) {
            reasons.push("runtime_activity_mtime_recent");
        }
        if is_recent_age(self.background_synthetic_activity_age_secs, freshness_secs) {
            reasons.push("background_synthetic_activity_recent");
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
    pub(super) inflight_age_secs: Option<u64>,
    pub(super) inflight_age_anchor_unix_secs: Option<i64>,
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
) -> StallWatchdogLivenessDecision {
    let key = StallLivenessKey::from_snapshot(provider, channel_id, snapshot);
    let evidence = StallWatchdogLivenessEvidence::collect(&key, snapshot, inflight, now_unix_secs);
    if !evidence.has_positive_liveness(freshness_secs) {
        DEFERRAL_STATE.remove(&key);
        return StallWatchdogLivenessDecision {
            action: StallWatchdogLivenessAction::ProceedNoEvidence,
            evidence,
            max_deferrals,
        };
    }

    let prior_deferrals = DEFERRAL_STATE
        .get(&key)
        .map(|state| state.count)
        .unwrap_or(0);
    if prior_deferrals >= max_deferrals {
        DEFERRAL_STATE.remove(&key);
        return StallWatchdogLivenessDecision {
            action: StallWatchdogLivenessAction::ProceedAfterDeferralLimit {
                previous_deferrals: prior_deferrals,
            },
            evidence,
            max_deferrals,
        };
    }

    let deferral_count = prior_deferrals.saturating_add(1);
    DEFERRAL_STATE.insert(
        key,
        DeferralState {
            count: deferral_count,
            last_updated_unix_secs: now_unix_secs,
        },
    );
    StallWatchdogLivenessDecision {
        action: StallWatchdogLivenessAction::Defer { deferral_count },
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
    DEFERRAL_STATE.retain(|key, _| !key.matches_session(&probe));
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

pub(super) fn gc_stall_watchdog_liveness_state(now_unix_secs: i64) {
    OFFSET_OBSERVATIONS.retain(|_, observation| {
        !liveness_state_expired(observation.last_updated_unix_secs, now_unix_secs)
    });
    DEFERRAL_STATE
        .retain(|_, state| !liveness_state_expired(state.last_updated_unix_secs, now_unix_secs));
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
        transcript_mtime_age_secs = ?decision.evidence.transcript_mtime_age_secs,
        runtime_activity_age_secs = ?decision.evidence.runtime_activity_age_secs,
        background_synthetic_activity_age_secs = ?decision.evidence.background_synthetic_activity_age_secs,
        background_synthetic_kind = ?decision.evidence.background_synthetic_kind,
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
    let limit_reached = decision.is_some_and(StallWatchdogLivenessDecision::limit_reached);
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
        liveness_deferral_limit_reached = limit_reached,
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
        let (pane_offset_previous, pane_offset_advanced_age_secs) =
            observe_pane_offset(key, snapshot.last_capture_offset, now_unix_secs);
        let background_synthetic =
            background_synthetic_activity_age_secs(snapshot, inflight, now_unix_secs);
        Self {
            pane_offset_current: snapshot.last_capture_offset,
            pane_offset_previous,
            pane_offset_advanced_age_secs,
            transcript_mtime_age_secs: transcript_mtime_age_secs(inflight, now_unix_secs),
            runtime_activity_age_secs: runtime_activity_age_secs(snapshot, now_unix_secs),
            background_synthetic_activity_age_secs: background_synthetic
                .as_ref()
                .map(|(_, age)| *age),
            background_synthetic_kind: background_synthetic.map(|(kind, _)| kind),
        }
    }
}

fn observe_pane_offset(
    key: &StallLivenessKey,
    current_offset: Option<u64>,
    now_unix_secs: i64,
) -> (Option<u64>, Option<u64>) {
    let Some(current_offset) = current_offset else {
        OFFSET_OBSERVATIONS.remove(key);
        return (None, None);
    };
    let previous = OFFSET_OBSERVATIONS.get(key).map(|entry| entry.clone());
    let advanced_at_unix_secs = match previous.as_ref() {
        Some(prev) if current_offset > prev.offset => Some(now_unix_secs),
        Some(prev) if current_offset == prev.offset => prev.advanced_at_unix_secs,
        _ => None,
    };
    OFFSET_OBSERVATIONS.insert(
        key.clone(),
        OffsetObservation {
            offset: current_offset,
            advanced_at_unix_secs,
            last_updated_unix_secs: now_unix_secs,
        },
    );
    (
        previous.map(|prev| prev.offset),
        advanced_at_unix_secs.map(|at| saturating_age_secs(at, now_unix_secs)),
    )
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
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use chrono::TimeZone;
    use poise::serenity_prelude::ChannelId;
    use tracing_subscriber::fmt::MakeWriter;

    use crate::services::discord::relay_health::{RelayHealthSnapshot, RelayStallState};

    use super::*;

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

    fn liveness_state_presence(key: &StallLivenessKey) -> (bool, bool) {
        (
            OFFSET_OBSERVATIONS.contains_key(key),
            DEFERRAL_STATE.contains_key(key),
        )
    }

    fn deferral_count(key: &StallLivenessKey) -> Option<u8> {
        DEFERRAL_STATE.get(key).map(|state| state.count)
    }

    #[test]
    fn positive_liveness_defers_cleanup_and_logs_reason() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3361);
        let tmux_session = "AgentDesk-codex-liveness-defers";
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
        );
        assert_eq!(
            decision.action,
            StallWatchdogLivenessAction::ProceedNoEvidence
        );
        assert!(!decision.should_defer());
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

    #[test]
    fn liveness_deferral_cap_allows_cleanup_after_max_passes_and_logs_limit() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3363);
        let tmux_session = "AgentDesk-codex-liveness-cap";
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let file = tempfile::NamedTempFile::new().expect("temp transcript");
        let inflight = inflight_with_output(
            channel.get(),
            tmux_session,
            Some(file.path().display().to_string()),
        );
        let snap = snapshot(channel.get(), tmux_session, Some(20));
        let now = chrono::Utc::now().timestamp();

        for expected_count in 1..=STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS {
            let decision = evaluate_stall_watchdog_liveness(
                &provider,
                channel,
                &snap,
                Some(&inflight),
                now,
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            );
            assert_eq!(
                decision.action,
                StallWatchdogLivenessAction::Defer {
                    deferral_count: expected_count
                }
            );
        }

        let decision = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
        );
        assert_eq!(
            decision.action,
            StallWatchdogLivenessAction::ProceedAfterDeferralLimit {
                previous_deferrals: STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS
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
            logs.contains("liveness_deferral_limit_reached=true"),
            "{logs}"
        );
    }

    #[test]
    fn liveness_deferrals_are_scoped_to_current_turn_identity() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3371);
        let tmux_session = "AgentDesk-codex-liveness-turn-identity";
        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        let file = tempfile::NamedTempFile::new().expect("temp transcript");
        let inflight = inflight_with_output(
            channel.get(),
            tmux_session,
            Some(file.path().display().to_string()),
        );
        let snap = snapshot(channel.get(), tmux_session, Some(20));
        let now = chrono::Utc::now().timestamp();

        for expected_count in 1..=2 {
            let decision = evaluate_stall_watchdog_liveness(
                &provider,
                channel,
                &snap,
                Some(&inflight),
                now,
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            );
            assert_eq!(
                decision.action,
                StallWatchdogLivenessAction::Defer {
                    deferral_count: expected_count
                }
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
        );

        assert_eq!(
            decision.action,
            StallWatchdogLivenessAction::Defer { deferral_count: 1 },
            "a new user_msg_id + started_at under the same tmux session gets a fresh deferral budget"
        );

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #3582 regression: the 2026-06-18 12:07 false-positive. A live turn that
    /// keeps emitting output (fresh transcript mtime every tick) was force-cleaned
    /// the instant the OLD cap of 3 was hit, even though `reason_codes` still
    /// listed positive liveness — a "Response sent" landed 5s later. With the cap
    /// raised to 20 the same strong-liveness streak that previously died at pass 4
    /// keeps deferring, so a live-but-quiet turn survives well past the old window.
    #[test]
    fn strong_liveness_past_old_cap_still_defers_under_new_cap() {
        const OLD_CAP: u8 = 3;
        assert!(
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS > OLD_CAP,
            "this regression only has teeth when the new cap exceeds the old one"
        );

        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3368);
        let tmux_session = "AgentDesk-codex-liveness-12-07";
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

        // Every tick from 1..=OLD_CAP+1 must STILL defer — at the old cap the
        // (OLD_CAP+1)th pass returned ProceedAfterDeferralLimit and killed the
        // live turn. Under the new cap it stays a Defer.
        for expected_count in 1..=(OLD_CAP + 1) {
            let decision = evaluate_stall_watchdog_liveness(
                &provider,
                channel,
                &snap,
                Some(&inflight),
                now,
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            );
            assert_eq!(
                decision.action,
                StallWatchdogLivenessAction::Defer {
                    deferral_count: expected_count
                },
                "pass {expected_count} should still defer under the raised cap"
            );
        }

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #3582 corollary: the cap raise must NOT weaken detection of a genuinely
    /// dead relay. When no liveness signal is present (`reason_codes == none`),
    /// the decision is `ProceedNoEvidence` on the very first tick regardless of
    /// the cap — exactly the 11:52 / 12:38 immediate-clean cases.
    #[test]
    fn no_liveness_still_proceeds_immediately_under_raised_cap() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3369);
        let tmux_session = "AgentDesk-codex-liveness-dead-relay";
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
        );
        assert_eq!(
            decision.action,
            StallWatchdogLivenessAction::ProceedNoEvidence,
            "a dead relay must be cleaned on the first tick even with a raised cap"
        );

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    #[test]
    fn liveness_deferral_streak_survives_desync_flap_without_positive_health() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3364);
        let tmux_session = "AgentDesk-codex-liveness-flap";
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

        // Build the streak up to one short of the cap.
        for expected_count in 1..STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS {
            let decision = evaluate_stall_watchdog_liveness(
                &provider,
                channel,
                &snap,
                Some(&inflight),
                now,
                STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
                STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
            );
            assert_eq!(
                decision.action,
                StallWatchdogLivenessAction::Defer {
                    deferral_count: expected_count
                }
            );
        }

        // A transient desync flap (desynced toggles off but terminal delivery
        // never committed) must NOT clear the in-flight deferral streak.
        let pre_flap_count = STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS - 1;
        let mut flapped_snapshot = snap.clone();
        flapped_snapshot.desynced = false;
        flapped_snapshot.relay_health.desynced = false;
        assert!(!clear_stall_watchdog_liveness_state_if_healthy(
            &provider,
            channel,
            &flapped_snapshot,
        ));
        assert_eq!(deferral_count(&key), Some(pre_flap_count));

        // The next tick reaches the cap (the final defer), then the one after
        // proceeds with the force-clean.
        let at_cap = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
        );
        assert_eq!(
            at_cap.action,
            StallWatchdogLivenessAction::Defer {
                deferral_count: STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS
            }
        );

        let decision = evaluate_stall_watchdog_liveness(
            &provider,
            channel,
            &snap,
            Some(&inflight),
            now,
            STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
        );
        assert_eq!(
            decision.action,
            StallWatchdogLivenessAction::ProceedAfterDeferralLimit {
                previous_deferrals: STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS
            }
        );

        clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    #[test]
    fn healthy_recovery_clears_all_liveness_state() {
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(3365);
        let tmux_session = "AgentDesk-codex-liveness-healthy-clear";
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
        );
        assert!(decision.should_defer());
        assert_eq!(liveness_state_presence(&key), (true, true));

        let mut healthy_snapshot = snap.clone();
        healthy_snapshot.inflight_terminal_delivery_committed = true;
        assert!(clear_stall_watchdog_liveness_state_if_healthy(
            &provider,
            channel,
            &healthy_snapshot,
        ));
        assert_eq!(liveness_state_presence(&key), (false, false));
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
        DEFERRAL_STATE.insert(
            old_key.clone(),
            DeferralState {
                count: 2,
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
        DEFERRAL_STATE.insert(
            fresh_key.clone(),
            DeferralState {
                count: 1,
                last_updated_unix_secs: fresh_at,
            },
        );

        gc_stall_watchdog_liveness_state(now);

        assert_eq!(liveness_state_presence(&old_key), (false, false));
        assert_eq!(liveness_state_presence(&fresh_key), (true, true));
        clear_stall_watchdog_liveness_state(&provider, fresh_channel, Some(fresh_tmux_session));
    }
}
