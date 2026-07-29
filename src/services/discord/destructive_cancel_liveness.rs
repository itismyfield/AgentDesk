use super::inflight;

pub(super) const ZERO_DELIVERY_FORFEIT_MIN_AGE_SECS: i64 = 1_800;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WatcherRelayLivenessEvidence<'a> {
    pub(super) output_len_at_snapshot: Option<u64>,
    pub(super) output_len_now: Option<u64>,
    pub(super) output_mtime_age_secs: Option<i64>,
    pub(super) relay_frontier_at_snapshot: Option<u64>,
    pub(super) relay_frontier_now: Option<u64>,
    pub(super) last_watcher_relayed_offset: Option<u64>,
    pub(super) last_watcher_relayed_at_unix: Option<i64>,
    pub(super) terminal_delivery_committed: bool,
    pub(super) full_response: &'a str,
    pub(super) response_sent_offset: usize,
    pub(super) prior_delivery_evidence: bool,
    pub(super) turn_age_secs: Option<i64>,
    pub(super) now_unix: i64,
}

pub(super) fn fresh_watcher_heartbeat_blocks_rebind(
    evidence: WatcherRelayLivenessEvidence<'_>,
) -> bool {
    watcher_relay_progress_recent(evidence) || !relay_liveness_forfeited(evidence)
}

pub(super) fn relay_liveness_forfeited(evidence: WatcherRelayLivenessEvidence<'_>) -> bool {
    let _producer_evidence = (
        evidence.output_len_at_snapshot,
        evidence.output_mtime_age_secs,
    );
    let unsent_response_payload_exists = !evidence.full_response.trim().is_empty()
        && evidence.response_sent_offset == 0
        && !evidence.prior_delivery_evidence;
    if !unsent_response_payload_exists {
        return false;
    }

    let zero_delivery_forfeited = evidence.last_watcher_relayed_offset.is_none()
        && !evidence.terminal_delivery_committed
        && evidence
            .turn_age_secs
            .is_some_and(|age| age >= ZERO_DELIVERY_FORFEIT_MIN_AGE_SECS);

    let stalled_relay_forfeited = match (
        evidence.last_watcher_relayed_offset,
        valid_elapsed_secs(evidence.last_watcher_relayed_at_unix, evidence.now_unix),
    ) {
        (Some(_), Some(stall_age_secs)) => {
            !relay_frontier_advanced(
                evidence.relay_frontier_at_snapshot,
                evidence.relay_frontier_now,
            ) && evidence.output_len_now.unwrap_or(0) > evidence.relay_frontier_now.unwrap_or(0)
                && stall_age_secs >= ZERO_DELIVERY_FORFEIT_MIN_AGE_SECS
        }
        _ => false,
    };

    zero_delivery_forfeited || stalled_relay_forfeited
}

fn valid_elapsed_secs(timestamp: Option<i64>, now_unix: i64) -> Option<i64> {
    let timestamp = timestamp?;
    (timestamp >= 0 && timestamp <= now_unix).then(|| now_unix - timestamp)
}

fn watcher_relay_progress_recent(evidence: WatcherRelayLivenessEvidence<'_>) -> bool {
    relay_frontier_advanced(
        evidence.relay_frontier_at_snapshot,
        evidence.relay_frontier_now,
    )
}

pub(super) fn relay_frontier_advanced(previous: Option<u64>, current: Option<u64>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => current > previous,
        (None, Some(current)) => current > 0,
        _ => false,
    }
}

pub(super) fn turn_age_secs(state: &inflight::InflightTurnState, now_unix: i64) -> Option<i64> {
    inflight::parse_started_at_unix(&state.started_at)
        .map(|started_at| now_unix.saturating_sub(started_at))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence<'a>() -> WatcherRelayLivenessEvidence<'a> {
        WatcherRelayLivenessEvidence {
            output_len_at_snapshot: Some(512_146),
            output_len_now: Some(512_147),
            output_mtime_age_secs: Some(0),
            relay_frontier_at_snapshot: Some(6_281_996),
            relay_frontier_now: Some(6_281_996),
            last_watcher_relayed_offset: None,
            last_watcher_relayed_at_unix: None,
            terminal_delivery_committed: false,
            full_response: "captured but unsent response",
            response_sent_offset: 0,
            prior_delivery_evidence: false,
            turn_age_secs: Some(ZERO_DELIVERY_FORFEIT_MIN_AGE_SECS + 1),
            now_unix: 100_000,
        }
    }

    #[test]
    fn destructive_cancel_producer_growth_does_not_prove_consumer_liveness() {
        assert!(relay_liveness_forfeited(evidence()));
        assert!(!fresh_watcher_heartbeat_blocks_rebind(evidence()));
    }

    #[test]
    fn destructive_cancel_consumer_progress_still_blocks_rebind() {
        let advanced = WatcherRelayLivenessEvidence {
            relay_frontier_now: Some(6_281_997),
            output_len_now: Some(512_146),
            output_mtime_age_secs: Some(601),
            ..evidence()
        };
        assert!(fresh_watcher_heartbeat_blocks_rebind(advanced));

        let first_advance = WatcherRelayLivenessEvidence {
            relay_frontier_at_snapshot: None,
            relay_frontier_now: Some(1),
            ..advanced
        };
        assert!(fresh_watcher_heartbeat_blocks_rebind(first_advance));
    }

    #[test]
    fn destructive_cancel_minimum_age_preserves_grace_window() {
        let within_grace = WatcherRelayLivenessEvidence {
            turn_age_secs: Some(ZERO_DELIVERY_FORFEIT_MIN_AGE_SECS - 1),
            ..evidence()
        };
        assert!(fresh_watcher_heartbeat_blocks_rebind(within_grace));
    }

    #[test]
    fn destructive_cancel_without_capture_divergence_does_not_forfeit() {
        let no_divergence = WatcherRelayLivenessEvidence {
            output_len_at_snapshot: Some(6_281_996),
            output_len_now: Some(6_281_996),
            last_watcher_relayed_offset: Some(6_281_996),
            last_watcher_relayed_at_unix: Some(90_000),
            ..evidence()
        };
        assert!(fresh_watcher_heartbeat_blocks_rebind(no_divergence));
    }

    #[test]
    fn destructive_cancel_legacy_relay_timestamp_absence_abstains() {
        let legacy = WatcherRelayLivenessEvidence {
            last_watcher_relayed_offset: Some(6_281_996),
            last_watcher_relayed_at_unix: None,
            output_len_now: Some(6_282_100),
            relay_frontier_at_snapshot: Some(6_281_996),
            relay_frontier_now: Some(6_281_996),
            now_unix: 200_000,
            turn_age_secs: Some(86_400),
            ..evidence()
        };
        assert!(!relay_liveness_forfeited(legacy));
        assert!(fresh_watcher_heartbeat_blocks_rebind(legacy));
    }

    #[test]
    fn destructive_cancel_rewound_offset_with_prior_delivery_abstains() {
        let rewound = WatcherRelayLivenessEvidence {
            response_sent_offset: 0,
            prior_delivery_evidence: true,
            ..evidence()
        };
        assert!(!relay_liveness_forfeited(rewound));
        assert!(fresh_watcher_heartbeat_blocks_rebind(rewound));
    }

    #[test]
    fn destructive_cancel_invalid_relay_timestamp_abstains() {
        for invalid in [Some(-1), Some(100_001)] {
            let invalid_timestamp = WatcherRelayLivenessEvidence {
                last_watcher_relayed_offset: Some(6_281_996),
                last_watcher_relayed_at_unix: invalid,
                output_len_now: Some(6_282_100),
                relay_frontier_at_snapshot: Some(6_281_996),
                relay_frontier_now: Some(6_281_996),
                now_unix: 100_000,
                ..evidence()
            };
            assert!(!relay_liveness_forfeited(invalid_timestamp));
            assert!(fresh_watcher_heartbeat_blocks_rebind(invalid_timestamp));
        }
    }

    #[test]
    fn destructive_cancel_tool_only_turn_abstains_independently_of_readiness() {
        let tool_only = WatcherRelayLivenessEvidence {
            full_response: "",
            ..evidence()
        };
        assert!(!relay_liveness_forfeited(tool_only));
        assert!(fresh_watcher_heartbeat_blocks_rebind(tool_only));
    }
}
