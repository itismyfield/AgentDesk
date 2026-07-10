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
    pub(super) producer_activity_recent: bool,
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
    ProducerActivityRecent,
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
            Self::ProducerActivityRecent => "producer_activity_recent",
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
    if signals.producer_activity_recent {
        live_reasons.push(StallVerdictReason::ProducerActivityRecent);
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

/// Adapter for the existing watchdog judgment logs. Producer progress comes
/// only from the evidence already computed for the existing decision; watcher
/// polling is consumer liveness and is deliberately excluded.
fn classify_existing_judgment(
    snapshot: &WatcherStateSnapshot,
    decision: Option<&super::stall_liveness::StallWatchdogLivenessDecision>,
    freshness_secs: u64,
) -> Option<StallVerdictAssessment> {
    classify_runtime_signals(
        &snapshot.relay_health,
        snapshot.attached,
        snapshot.inflight_state_present,
        snapshot.inflight_terminal_delivery_committed,
        decision.is_some_and(|decision| decision.evidence.has_positive_liveness(freshness_secs)),
        false,
        false,
        snapshot.desynced,
    )
}

pub(super) fn classify_health_snapshot_lossy(
    provider: Option<&ProviderKind>,
    channel_id: ChannelId,
    session: &SessionEnrichment,
    relay: &RelayHealthSnapshot,
    boot_unix_secs: i64,
) -> Option<StallVerdict> {
    classify_health_snapshot_at_lossy(
        provider,
        channel_id,
        session,
        relay,
        chrono::Utc::now().timestamp(),
        boot_unix_secs,
    )
    .map(|assessment| assessment.verdict)
}

fn classify_health_snapshot_at_lossy(
    provider: Option<&ProviderKind>,
    channel_id: ChannelId,
    session: &SessionEnrichment,
    relay: &RelayHealthSnapshot,
    now_unix_secs: i64,
    boot_unix_secs: i64,
) -> Option<StallVerdictAssessment> {
    let provider = provider?;
    let desynced = session.desynced(relay.tmux_alive == Some(true), session.attached);
    if !classification_is_applicable(
        relay,
        session.attached,
        session.inflight_state_present,
        session.inflight_terminal_delivery_committed(),
        desynced,
    ) {
        return None;
    }

    let producer_activity_recent = match recent_local_timestamp(
        session
            .inflight
            .as_ref()
            .map(|state| state.updated_at.as_str()),
        "inflight_updated_at",
        now_unix_secs,
        super::stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
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
    let frontier_advanced_recently = recent_unix_millis(
        relay.last_relay_ts_ms,
        now_unix_secs,
        super::stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
    );
    let restart_grace_active = session.inflight_state_present
        && now_unix_secs >= boot_unix_secs
        && now_unix_secs.saturating_sub(boot_unix_secs) as u64
            <= super::recovery::STALL_WATCHDOG_THRESHOLD_SECS;
    classify_runtime_signals(
        relay,
        session.attached,
        session.inflight_state_present,
        session.inflight_terminal_delivery_committed(),
        producer_activity_recent,
        frontier_advanced_recently,
        restart_grace_active,
        desynced,
    )
}

fn classification_is_applicable(
    relay: &RelayHealthSnapshot,
    channel_session_attached: bool,
    inflight_state_present: bool,
    delivery_committed: bool,
    desynced: bool,
) -> bool {
    inflight_state_present
        || delivery_committed
        || desynced
        || relay.mailbox_has_cancel_token
        || channel_session_attached
}

#[allow(clippy::too_many_arguments)]
fn classify_runtime_signals(
    relay: &RelayHealthSnapshot,
    channel_session_attached: bool,
    inflight_state_present: bool,
    delivery_committed: bool,
    producer_activity_recent: bool,
    frontier_advanced_recently: bool,
    restart_grace_active: bool,
    desynced: bool,
) -> Option<StallVerdictAssessment> {
    if !classification_is_applicable(
        relay,
        channel_session_attached,
        inflight_state_present,
        delivery_committed,
        desynced,
    ) {
        return None;
    }

    let phantom_attached = channel_session_attached
        && (relay.watcher_attached_stale || relay.tmux_alive == Some(false));
    let idle =
        !relay.mailbox_has_cancel_token && matches!(relay.active_turn, RelayActiveTurn::None);

    Some(classify_stall(StallSignalSnapshot {
        producer_activity_recent,
        frontier_advanced_recently,
        desynced,
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
    snapshot: &WatcherStateSnapshot,
    decision: Option<&super::stall_liveness::StallWatchdogLivenessDecision>,
    freshness_secs: u64,
) -> (&'static str, String) {
    let assessment = classify_existing_judgment(snapshot, decision, freshness_secs);
    classification_log_fields(assessment.as_ref())
}

#[cfg(test)]
mod tests {
    use super::super::mailbox::MailboxHealthSnapshot;
    use super::super::stall_liveness::{
        StallWatchdogLivenessAction, StallWatchdogLivenessDecision, StallWatchdogLivenessEvidence,
    };
    use super::*;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::discord::relay_health::RelayStallState;

    const FIXTURE_UPDATED_AT: &str = "2026-07-11 12:00:00";

    fn quiet_signals() -> StallSignalSnapshot {
        StallSignalSnapshot {
            producer_activity_recent: false,
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

    fn fixture_updated_at_unix() -> i64 {
        crate::services::discord::inflight::parse_updated_at_unix(FIXTURE_UPDATED_AT)
            .expect("fixture timestamp")
    }

    fn relay_fixture() -> RelayHealthSnapshot {
        RelayHealthSnapshot {
            provider: ProviderKind::Codex.as_str().to_string(),
            channel_id: 42,
            active_turn: RelayActiveTurn::Foreground,
            tmux_session: Some("AgentDesk-codex-fixture".to_string()),
            tmux_alive: Some(true),
            watcher_attached: true,
            watcher_attached_stale: false,
            watcher_owner_channel_id: Some(42),
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
            last_relay_ts_ms: None,
            last_outbound_activity_ms: None,
            last_capture_offset: Some(20),
            last_relay_offset: 10,
            unread_bytes: Some(10),
            desynced: true,
            stale_thread_proof: false,
        }
    }

    fn inflight_fixture(updated_at: &str) -> InflightTurnState {
        serde_json::from_value(serde_json::json!({
            "version": 9,
            "provider": "codex",
            "channel_id": 42,
            "channel_name": "fixture",
            "request_owner_user_id": 1,
            "user_msg_id": 9001,
            "current_msg_id": 9002,
            "current_msg_len": 0,
            "user_text": "fixture",
            "source": "text",
            "session_id": "session",
            "tmux_session_name": "AgentDesk-codex-fixture",
            "output_path": "/tmp/stall-verdict-fixture.jsonl",
            "input_fifo_path": null,
            "last_offset": 10,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": FIXTURE_UPDATED_AT,
            "updated_at": updated_at,
            "watcher_owns_live_relay": true
        }))
        .expect("deserialize raw inflight fixture")
    }

    fn session_fixture(updated_at: Option<&str>, attached: bool) -> SessionEnrichment {
        let inflight = updated_at.map(inflight_fixture);
        SessionEnrichment {
            inflight,
            attached,
            watcher_attached: attached,
            watcher_attached_stale: false,
            has_relay_coord: attached,
            watcher_owner_channel_id: attached.then_some(42),
            tmux_session: attached.then(|| "AgentDesk-codex-fixture".to_string()),
            inflight_state_present: updated_at.is_some(),
            tmux_session_mismatch: false,
            last_relay_offset: 10,
            last_relay_ts_ms: 0,
            reconnect_count: 0,
            last_capture_offset: Some(20),
            unread_bytes: Some(10),
            relay_stale: true,
            capture_lagged: false,
        }
    }

    fn watcher_fixture() -> WatcherStateSnapshot {
        WatcherStateSnapshot {
            provider: ProviderKind::Codex.as_str().to_string(),
            attached: true,
            tmux_session: Some("AgentDesk-codex-fixture".to_string()),
            watcher_owner_channel_id: Some(42),
            last_relay_offset: 10,
            inflight_state_present: true,
            last_relay_ts_ms: 0,
            last_capture_offset: Some(20),
            unread_bytes: Some(10),
            desynced: true,
            reconnect_count: 0,
            inflight_started_at: Some(FIXTURE_UPDATED_AT.to_string()),
            inflight_updated_at: Some(FIXTURE_UPDATED_AT.to_string()),
            inflight_user_msg_id: Some(9001),
            inflight_current_msg_id: Some(9002),
            tmux_session_alive: Some(true),
            has_pending_queue: false,
            mailbox_active_user_msg_id: Some(9001),
            bound_output_path: None,
            bound_session_id: None,
            inflight_terminal_delivery_committed: false,
            inflight_identity: None,
            inflight_finalizer_turn_id: None,
            inflight_output_path: Some("/tmp/stall-verdict-fixture.jsonl".to_string()),
            relay_stall_state: RelayStallState::TmuxAliveRelayDead,
            relay_health: relay_fixture(),
        }
    }

    fn liveness_decision(
        action: StallWatchdogLivenessAction,
        evidence: StallWatchdogLivenessEvidence,
    ) -> StallWatchdogLivenessDecision {
        StallWatchdogLivenessDecision {
            action,
            evidence,
            max_deferrals: 0,
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
    fn producer_activity_advancing_while_desynced_is_producer_live() {
        let assessment = classify_stall(StallSignalSnapshot {
            producer_activity_recent: true,
            desynced: true,
            ..quiet_signals()
        });
        assert_eq!(assessment.verdict, StallVerdict::ProducerLive);
        assert_eq!(
            assessment.reasons,
            vec![StallVerdictReason::ProducerActivityRecent]
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
    fn raw_hung_producer_with_fresh_watcher_is_control_plane_desync() {
        let snapshot = watcher_fixture();
        assert!(snapshot.attached);
        assert!(snapshot.relay_health.watcher_attached);
        assert!(!snapshot.relay_health.watcher_attached_stale);
        assert_eq!(snapshot.tmux_session_alive, Some(true));
        let decision = liveness_decision(
            StallWatchdogLivenessAction::ProceedNoEvidence,
            StallWatchdogLivenessEvidence::default(),
        );

        let (verdict, reasons) = judgment_log_fields(
            &snapshot,
            Some(&decision),
            super::super::stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
        );

        assert_eq!(verdict, "control_plane_desync");
        assert_eq!(reasons, "desynced,mailbox_cancel_token_present");
    }

    #[test]
    fn judgment_log_fields_uses_each_established_decision_evidence_kind() {
        let evidence_cases = [
            StallWatchdogLivenessEvidence {
                pane_offset_advanced_age_secs: Some(1),
                ..Default::default()
            },
            StallWatchdogLivenessEvidence {
                relay_offset_advanced_age_secs: Some(1),
                ..Default::default()
            },
            StallWatchdogLivenessEvidence {
                transcript_mtime_age_secs: Some(1),
                ..Default::default()
            },
            StallWatchdogLivenessEvidence {
                runtime_activity_age_secs: Some(1),
                ..Default::default()
            },
            StallWatchdogLivenessEvidence {
                outbound_activity_age_secs: Some(1),
                ..Default::default()
            },
            StallWatchdogLivenessEvidence {
                background_synthetic_activity_age_secs: Some(1),
                ..Default::default()
            },
            StallWatchdogLivenessEvidence {
                has_undelivered_backlog: true,
                ..Default::default()
            },
            StallWatchdogLivenessEvidence {
                open_tool_execution_age_secs: Some(1),
                ..Default::default()
            },
        ];
        let snapshot = watcher_fixture();

        for evidence in evidence_cases {
            let decision = liveness_decision(
                StallWatchdogLivenessAction::Defer { deferral_count: 0 },
                evidence,
            );
            let (verdict, reasons) = judgment_log_fields(
                &snapshot,
                Some(&decision),
                super::super::stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS,
            );
            assert_eq!(verdict, "producer_live");
            assert_eq!(reasons, "producer_activity_recent");
        }
    }

    #[test]
    fn raw_incident_4423_phantom_and_pretripped_token_is_control_plane_desync() {
        let freshness = super::super::stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS;
        let now = fixture_updated_at_unix() + freshness as i64 + 10;
        let mut session = session_fixture(Some(FIXTURE_UPDATED_AT), true);
        // `attached` can come from the inflight tmux owner even when this
        // channel has no strict watcher binding. Verdict adapters intentionally
        // use this same channel/session fact as watchdog snapshots.
        session.watcher_attached = false;
        session.capture_lagged = true;
        let mut relay = relay_fixture();
        relay.watcher_attached = false;
        relay.tmux_alive = Some(false);

        let assessment = classify_health_snapshot_at_lossy(
            Some(&ProviderKind::Codex),
            ChannelId::new(42),
            &session,
            &relay,
            now,
            now - super::super::recovery::STALL_WATCHDOG_THRESHOLD_SECS as i64 - 1,
        )
        .expect("applicable #4423 fixture");

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
    fn raw_health_fresh_inflight_or_relay_frontier_is_producer_live() {
        let freshness = super::super::stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS;
        let timestamp = fixture_updated_at_unix();
        let fresh_now = timestamp + 1;
        let session = session_fixture(Some(FIXTURE_UPDATED_AT), true);
        let relay = relay_fixture();
        let fresh_inflight = classify_health_snapshot_at_lossy(
            Some(&ProviderKind::Codex),
            ChannelId::new(42),
            &session,
            &relay,
            fresh_now,
            fresh_now - super::super::recovery::STALL_WATCHDOG_THRESHOLD_SECS as i64 - 1,
        )
        .expect("fresh inflight fixture");
        assert_eq!(fresh_inflight.verdict, StallVerdict::ProducerLive);
        assert_eq!(
            fresh_inflight.reasons,
            vec![StallVerdictReason::ProducerActivityRecent]
        );

        let stale_now = timestamp + freshness as i64 + 10;
        let mut relay = relay_fixture();
        relay.last_relay_ts_ms = Some(stale_now.saturating_mul(1000));
        let fresh_frontier = classify_health_snapshot_at_lossy(
            Some(&ProviderKind::Codex),
            ChannelId::new(42),
            &session,
            &relay,
            stale_now,
            stale_now - super::super::recovery::STALL_WATCHDOG_THRESHOLD_SECS as i64 - 1,
        )
        .expect("fresh relay frontier fixture");
        assert_eq!(fresh_frontier.verdict, StallVerdict::ProducerLive);
        assert_eq!(
            fresh_frontier.reasons,
            vec![StallVerdictReason::FrontierAdvancedRecently]
        );
    }

    #[test]
    fn raw_health_non_applicable_channel_returns_none() {
        let session = session_fixture(None, false);
        let mut relay = relay_fixture();
        relay.active_turn = RelayActiveTurn::None;
        relay.tmux_session = None;
        relay.tmux_alive = None;
        relay.watcher_attached = false;
        relay.watcher_owner_channel_id = None;
        relay.watcher_owns_live_relay = false;
        relay.bridge_inflight_present = false;
        relay.bridge_current_msg_id = None;
        relay.mailbox_has_cancel_token = false;
        relay.mailbox_active_user_msg_id = None;
        relay.pending_discord_callback_msg_id = None;
        relay.last_capture_offset = None;
        relay.unread_bytes = None;
        relay.desynced = false;

        assert_eq!(
            classify_health_snapshot_at_lossy(
                Some(&ProviderKind::Codex),
                ChannelId::new(42),
                &session,
                &relay,
                fixture_updated_at_unix(),
                fixture_updated_at_unix(),
            ),
            None
        );
    }

    #[test]
    fn raw_health_malformed_updated_at_fails_open_to_null() {
        let session = session_fixture(Some("not-a-timestamp"), true);
        let relay = relay_fixture();

        assert_eq!(
            classify_health_snapshot_at_lossy(
                Some(&ProviderKind::Codex),
                ChannelId::new(42),
                &session,
                &relay,
                fixture_updated_at_unix(),
                fixture_updated_at_unix(),
            ),
            None
        );
    }

    #[test]
    fn raw_health_restart_grace_boundary_comes_from_boot_timestamp() {
        let freshness = super::super::stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS;
        let threshold = super::super::recovery::STALL_WATCHDOG_THRESHOLD_SECS;
        let now = fixture_updated_at_unix() + freshness as i64 + 10;
        let mut session = session_fixture(Some(FIXTURE_UPDATED_AT), true);
        session.capture_lagged = true;
        let relay = relay_fixture();

        let inside = classify_health_snapshot_at_lossy(
            Some(&ProviderKind::Codex),
            ChannelId::new(42),
            &session,
            &relay,
            now,
            now - threshold as i64,
        )
        .expect("inside restart grace");
        assert_eq!(inside.verdict, StallVerdict::ProducerLive);
        assert_eq!(inside.reasons, vec![StallVerdictReason::RestartGraceActive]);

        let outside = classify_health_snapshot_at_lossy(
            Some(&ProviderKind::Codex),
            ChannelId::new(42),
            &session,
            &relay,
            now,
            now - threshold as i64 - 1,
        )
        .expect("outside restart grace remains applicable");
        assert_eq!(outside.verdict, StallVerdict::ControlPlaneDesync);
        assert!(
            !outside
                .reasons
                .contains(&StallVerdictReason::RestartGraceActive)
        );
    }

    #[test]
    fn verdict_serializes_for_health_detail() {
        assert_eq!(
            serde_json::to_value(StallVerdict::ControlPlaneDesync).unwrap(),
            serde_json::json!("control_plane_desync")
        );
    }

    #[test]
    fn health_detail_mailbox_serializes_unavailable_shadow_verdict_as_null() {
        let mailbox = MailboxHealthSnapshot {
            provider: ProviderKind::Codex.as_str().to_string(),
            channel_id: 42,
            has_cancel_token: false,
            queue_depth: 0,
            recovery_started: false,
            active_request_owner: None,
            active_user_message_id: None,
            agent_turn_status: "idle",
            watcher_attached: false,
            inflight_state_present: false,
            tmux_present: false,
            process_present: false,
            active_dispatch_present: false,
            stall_shadow_verdict: None,
            relay_stall_state: RelayStallState::Healthy,
            relay_health: relay_fixture(),
        };

        let serialized = serde_json::to_value(mailbox).expect("serialize health mailbox");
        assert_eq!(serialized["stall_shadow_verdict"], serde_json::Value::Null);
    }
}
