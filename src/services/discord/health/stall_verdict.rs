//! #4254 W0: side-effect-free stall classification for shadow telemetry.
//!
//! This module is deliberately not consulted by recovery. It translates the
//! signals the stall watchdog already observes into a parallel verdict so W2
//! can be gated on incident data before any verdict becomes authoritative.

use std::fmt;

use poise::serenity_prelude::ChannelId;
use serde::Serialize;

use super::session_enrichment::SessionEnrichment;
use super::snapshot::WatcherStateSnapshot;
use crate::services::discord::relay_health::{RelayActiveTurn, RelayHealthSnapshot};
use crate::services::provider::ProviderKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StallVerdict {
    ProducerLive,
    ControlPlaneDesync,
    ProducerDead,
    DeliveredIdle,
}

impl StallVerdict {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::ProducerLive => "producer_live",
            Self::ControlPlaneDesync => "control_plane_desync",
            Self::ProducerDead => "producer_dead",
            Self::DeliveredIdle => "delivered_idle",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StallSignalSnapshot {
    pub(super) producer_heartbeat_recent: bool,
    pub(super) frontier_advanced_recently: bool,
    pub(super) desynced: bool,
    pub(super) mailbox_cancel_token_present: bool,
    pub(super) phantom_attached: bool,
    pub(super) producer_known_dead: bool,
    pub(super) delivery_committed: bool,
    pub(super) idle: bool,
    pub(super) restart_grace_active: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StallVerdictReason {
    DeliveryCommitted,
    Idle,
    ProducerHeartbeatRecent,
    FrontierAdvancedRecently,
    RestartGraceActive,
    Desynced,
    MailboxCancelTokenPresent,
    PhantomAttached,
    ProducerKnownDead,
    NoPositiveLiveness,
}

impl StallVerdictReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::DeliveryCommitted => "delivery_committed",
            Self::Idle => "idle",
            Self::ProducerHeartbeatRecent => "producer_heartbeat_recent",
            Self::FrontierAdvancedRecently => "frontier_advanced_recently",
            Self::RestartGraceActive => "restart_grace_active",
            Self::Desynced => "desynced",
            Self::MailboxCancelTokenPresent => "mailbox_cancel_token_present",
            Self::PhantomAttached => "phantom_attached",
            Self::ProducerKnownDead => "producer_known_dead",
            Self::NoPositiveLiveness => "no_positive_liveness",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StallVerdictAssessment {
    pub(super) verdict: StallVerdict,
    pub(super) reasons: Vec<StallVerdictReason>,
}

impl StallVerdictAssessment {
    fn new(verdict: StallVerdict, reasons: Vec<StallVerdictReason>) -> Self {
        Self { verdict, reasons }
    }

    fn reason_codes_csv(&self) -> String {
        self.reasons
            .iter()
            .map(|reason| reason.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Pure W0 classifier. Ordering is intentional: completed idle work and
/// positive producer evidence outrank every desync symptom; control-plane
/// contamination outranks a dead-producer fallback.
pub(super) fn classify_stall(signals: StallSignalSnapshot) -> StallVerdictAssessment {
    if signals.delivery_committed && signals.idle {
        return StallVerdictAssessment::new(
            StallVerdict::DeliveredIdle,
            vec![
                StallVerdictReason::DeliveryCommitted,
                StallVerdictReason::Idle,
            ],
        );
    }

    let mut live_reasons = Vec::new();
    if signals.producer_heartbeat_recent {
        live_reasons.push(StallVerdictReason::ProducerHeartbeatRecent);
    }
    if signals.frontier_advanced_recently {
        live_reasons.push(StallVerdictReason::FrontierAdvancedRecently);
    }
    if signals.restart_grace_active {
        live_reasons.push(StallVerdictReason::RestartGraceActive);
    }
    if !live_reasons.is_empty() {
        return StallVerdictAssessment::new(StallVerdict::ProducerLive, live_reasons);
    }

    if signals.desynced && (signals.mailbox_cancel_token_present || signals.phantom_attached) {
        let mut reasons = vec![StallVerdictReason::Desynced];
        if signals.mailbox_cancel_token_present {
            reasons.push(StallVerdictReason::MailboxCancelTokenPresent);
        }
        if signals.phantom_attached {
            reasons.push(StallVerdictReason::PhantomAttached);
        }
        return StallVerdictAssessment::new(StallVerdict::ControlPlaneDesync, reasons);
    }

    if signals.producer_known_dead {
        return StallVerdictAssessment::new(
            StallVerdict::ProducerDead,
            vec![StallVerdictReason::ProducerKnownDead],
        );
    }

    if signals.desynced {
        return StallVerdictAssessment::new(
            StallVerdict::ControlPlaneDesync,
            vec![StallVerdictReason::Desynced],
        );
    }

    StallVerdictAssessment::new(
        StallVerdict::ProducerDead,
        vec![StallVerdictReason::NoPositiveLiveness],
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SignalParseError {
    field: &'static str,
}

impl fmt::Display for SignalParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid {} timestamp", self.field)
    }
}

fn recent_local_timestamp(
    raw: Option<&str>,
    field: &'static str,
    now_unix_secs: i64,
    freshness_secs: u64,
) -> Result<bool, SignalParseError> {
    let Some(raw) = raw else {
        return Ok(false);
    };
    let timestamp = crate::services::discord::inflight::parse_updated_at_unix(raw)
        .ok_or(SignalParseError { field })?;
    Ok(now_unix_secs.saturating_sub(timestamp).max(0) as u64 <= freshness_secs)
}

fn recent_unix_millis(timestamp_ms: Option<i64>, now_unix_secs: i64, freshness_secs: u64) -> bool {
    timestamp_ms.is_some_and(|timestamp_ms| {
        let now_ms = now_unix_secs.saturating_mul(1000);
        now_ms.saturating_sub(timestamp_ms).max(0) as u64 <= freshness_secs.saturating_mul(1000)
    })
}

/// Adapter for the existing watchdog judgment logs. A failure only suppresses
/// the shadow fields; it cannot alter the already-made recovery decision.
pub(super) fn classify_existing_judgment_lossy(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    freshness_secs: u64,
    frontier_advanced_recently: bool,
) -> Option<StallVerdictAssessment> {
    classify_runtime_signals_lossy(
        provider,
        channel_id,
        &snapshot.relay_health,
        snapshot.inflight_state_present,
        snapshot.inflight_updated_at.as_deref(),
        snapshot.inflight_terminal_delivery_committed,
        chrono::Utc::now().timestamp(),
        freshness_secs,
        false,
        frontier_advanced_recently,
    )
}

pub(super) fn classify_health_snapshot_lossy(
    provider: Option<&ProviderKind>,
    channel_id: ChannelId,
    session: &SessionEnrichment,
    relay: &RelayHealthSnapshot,
    boot_unix_secs: i64,
) -> Option<StallVerdict> {
    let provider = provider?;
    let now_unix_secs = chrono::Utc::now().timestamp();
    let restart_grace_active = session.inflight_state_present
        && now_unix_secs >= boot_unix_secs
        && now_unix_secs.saturating_sub(boot_unix_secs) as u64
            <= super::recovery::STALL_WATCHDOG_THRESHOLD_SECS;
    classify_runtime_signals_lossy(
        provider,
        channel_id,
        relay,
        session.inflight_state_present,
        session
            .inflight
            .as_ref()
            .map(|state| state.updated_at.as_str()),
        session.inflight_terminal_delivery_committed(),
        now_unix_secs,
        super::stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
        restart_grace_active,
        false,
    )
    .map(|assessment| assessment.verdict)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn classify_runtime_signals_lossy(
    provider: &ProviderKind,
    channel_id: ChannelId,
    relay: &RelayHealthSnapshot,
    inflight_state_present: bool,
    inflight_updated_at: Option<&str>,
    delivery_committed: bool,
    now_unix_secs: i64,
    freshness_secs: u64,
    restart_grace_active: bool,
    frontier_advanced_this_tick: bool,
) -> Option<StallVerdictAssessment> {
    let applicable = inflight_state_present
        || delivery_committed
        || relay.desynced
        || relay.mailbox_has_cancel_token
        || relay.watcher_attached;
    if !applicable {
        return None;
    }

    let inflight_heartbeat_recent = match recent_local_timestamp(
        inflight_updated_at,
        "inflight_updated_at",
        now_unix_secs,
        freshness_secs,
    ) {
        Ok(recent) => recent,
        Err(error) => {
            tracing::warn!(
                event = "stall_shadow_verdict_signal_error",
                provider = provider.as_str(),
                channel_id = channel_id.get(),
                error = %error,
                "STALL-WATCHDOG shadow verdict signal parse failed; ignoring telemetry"
            );
            return None;
        }
    };
    let watcher_heartbeat_recent =
        relay.watcher_attached && !relay.watcher_attached_stale && relay.tmux_alive != Some(false);
    let frontier_advanced_recently = frontier_advanced_this_tick
        || recent_unix_millis(relay.last_relay_ts_ms, now_unix_secs, freshness_secs);
    let phantom_attached =
        relay.watcher_attached && (relay.watcher_attached_stale || relay.tmux_alive == Some(false));
    let idle =
        !relay.mailbox_has_cancel_token && matches!(relay.active_turn, RelayActiveTurn::None);

    Some(classify_stall(StallSignalSnapshot {
        producer_heartbeat_recent: inflight_heartbeat_recent || watcher_heartbeat_recent,
        frontier_advanced_recently,
        desynced: relay.desynced,
        mailbox_cancel_token_present: relay.mailbox_has_cancel_token,
        phantom_attached,
        producer_known_dead: relay.tmux_alive == Some(false),
        delivery_committed,
        idle,
        restart_grace_active,
    }))
}

pub(super) fn classification_log_fields(
    assessment: Option<&StallVerdictAssessment>,
) -> (&'static str, String) {
    let shadow_verdict = assessment
        .map(|assessment| assessment.verdict.as_str())
        .unwrap_or("unavailable");
    let shadow_reasons = assessment
        .map(StallVerdictAssessment::reason_codes_csv)
        .unwrap_or_else(|| "none".to_string());
    (shadow_verdict, shadow_reasons)
}

pub(super) fn judgment_log_fields(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    decision: Option<&super::stall_liveness::StallWatchdogLivenessDecision>,
    freshness_secs: u64,
) -> (&'static str, String) {
    let frontier_advanced_recently = decision.is_some_and(|decision| {
        decision
            .evidence
            .pane_offset_advanced_age_secs
            .is_some_and(|age| age <= freshness_secs)
            || decision
                .evidence
                .relay_offset_advanced_age_secs
                .is_some_and(|age| age <= freshness_secs)
    });
    let assessment = classify_existing_judgment_lossy(
        provider,
        channel_id,
        snapshot,
        freshness_secs,
        frontier_advanced_recently,
    );
    classification_log_fields(assessment.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet_signals() -> StallSignalSnapshot {
        StallSignalSnapshot {
            producer_heartbeat_recent: false,
            frontier_advanced_recently: false,
            desynced: false,
            mailbox_cancel_token_present: false,
            phantom_attached: false,
            producer_known_dead: false,
            delivery_committed: false,
            idle: false,
            restart_grace_active: false,
        }
    }

    #[test]
    fn incident_4423_phantom_attached_with_pretripped_token_is_control_plane_desync() {
        let assessment = classify_stall(StallSignalSnapshot {
            desynced: true,
            mailbox_cancel_token_present: true,
            phantom_attached: true,
            ..quiet_signals()
        });
        assert_eq!(assessment.verdict, StallVerdict::ControlPlaneDesync);
        assert_eq!(
            assessment.reasons,
            vec![
                StallVerdictReason::Desynced,
                StallVerdictReason::MailboxCancelTokenPresent,
                StallVerdictReason::PhantomAttached,
            ]
        );
    }

    #[test]
    fn deploy_restart_first_turn_window_is_producer_live() {
        let assessment = classify_stall(StallSignalSnapshot {
            desynced: true,
            mailbox_cancel_token_present: true,
            restart_grace_active: true,
            ..quiet_signals()
        });
        assert_eq!(assessment.verdict, StallVerdict::ProducerLive);
        assert_eq!(
            assessment.reasons,
            vec![StallVerdictReason::RestartGraceActive]
        );
    }

    #[test]
    fn heartbeat_advancing_while_desynced_is_producer_live() {
        let assessment = classify_stall(StallSignalSnapshot {
            producer_heartbeat_recent: true,
            desynced: true,
            ..quiet_signals()
        });
        assert_eq!(assessment.verdict, StallVerdict::ProducerLive);
        assert_eq!(
            assessment.reasons,
            vec![StallVerdictReason::ProducerHeartbeatRecent]
        );
    }

    #[test]
    fn delivered_then_idle_is_delivered_idle() {
        let assessment = classify_stall(StallSignalSnapshot {
            delivery_committed: true,
            idle: true,
            producer_known_dead: true,
            ..quiet_signals()
        });
        assert_eq!(assessment.verdict, StallVerdict::DeliveredIdle);
        assert_eq!(
            assessment.reasons,
            vec![
                StallVerdictReason::DeliveryCommitted,
                StallVerdictReason::Idle,
            ]
        );
    }

    #[test]
    fn truth_table_covers_frontier_desync_dead_and_desync_only() {
        let cases = [
            (
                StallSignalSnapshot {
                    frontier_advanced_recently: true,
                    desynced: true,
                    ..quiet_signals()
                },
                StallVerdict::ProducerLive,
            ),
            (
                StallSignalSnapshot {
                    producer_known_dead: true,
                    ..quiet_signals()
                },
                StallVerdict::ProducerDead,
            ),
            (
                StallSignalSnapshot {
                    desynced: true,
                    ..quiet_signals()
                },
                StallVerdict::ControlPlaneDesync,
            ),
            (quiet_signals(), StallVerdict::ProducerDead),
        ];
        for (signals, expected) in cases {
            assert_eq!(classify_stall(signals).verdict, expected);
        }
    }

    #[test]
    fn verdict_serializes_for_health_detail() {
        assert_eq!(
            serde_json::to_value(StallVerdict::ControlPlaneDesync).unwrap(),
            serde_json::json!("control_plane_desync")
        );
    }
}
