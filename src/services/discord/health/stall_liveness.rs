use std::sync::LazyLock;

use poise::serenity_prelude::ChannelId;

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::relay_health::RelayActiveTurn;
use crate::services::provider::ProviderKind;

use super::snapshot::WatcherStateSnapshot;

pub(super) const STALL_WATCHDOG_POSITIVE_LIVENESS_SECS: u64 = 120;
pub(super) const STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS: u8 = 3;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct StallLivenessKey {
    provider: String,
    channel_id: u64,
    tmux_session: Option<String>,
}

#[derive(Clone, Debug)]
struct OffsetObservation {
    offset: u64,
    advanced_at_unix_secs: Option<i64>,
}

#[derive(Clone, Debug)]
struct DeferralState {
    count: u8,
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
        let updated_at_unix = snapshot
            .inflight_updated_at
            .as_deref()
            .and_then(crate::services::discord::inflight::parse_updated_at_unix);
        let inflight_age_anchor_unix_secs =
            updated_at_unix.map(|updated| updated.max(boot_unix_secs));
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
    let key = StallLivenessKey::new(provider, channel_id, snapshot.tmux_session.as_deref());
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
        },
    );
    StallWatchdogLivenessDecision {
        action: StallWatchdogLivenessAction::Defer { deferral_count },
        evidence,
        max_deferrals,
    }
}

pub(super) fn clear_stall_watchdog_liveness_deferral(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session: Option<&str>,
) {
    DEFERRAL_STATE.remove(&StallLivenessKey::new(provider, channel_id, tmux_session));
}

pub(super) fn clear_stall_watchdog_liveness_state(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session: Option<&str>,
) {
    let key = StallLivenessKey::new(provider, channel_id, tmux_session);
    DEFERRAL_STATE.remove(&key);
    OFFSET_OBSERVATIONS.remove(&key);
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
    fn new(provider: &ProviderKind, channel_id: ChannelId, tmux_session: Option<&str>) -> Self {
        Self {
            provider: provider.as_str().to_string(),
            channel_id: channel_id.get(),
            tmux_session: tmux_session.map(str::to_string),
        }
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
    fn liveness_deferral_cap_allows_cleanup_after_three_passes_and_logs_limit() {
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
}
