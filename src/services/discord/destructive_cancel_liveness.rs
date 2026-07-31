use super::inflight;

// Conservative, currently unmeasured threshold inherited from #4992 rollout;
// would-forfeit remains observation-only pending calibrated #5007 activation.
pub(super) const RELAY_FORFEIT_MIN_AGE_SECS: i64 = 1_800; // 30 minutes

// A persisted wall-clock timestamp this far behind the current process cannot
// describe a live turn. Rejecting it keeps a legacy/epoch `started_at` from
// making the `stall_age <= turn_age` consistency check vacuous.
pub(super) const MAX_RELAY_LIVENESS_TURN_AGE_SECS: i64 = 7 * 24 * 60 * 60; // 7 days

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RelayForfeitArm {
    ZeroDelivery,
    StalledDelivery,
}

pub(super) fn relay_forfeit_arm(prior_delivery_evidence: bool) -> RelayForfeitArm {
    if prior_delivery_evidence {
        RelayForfeitArm::StalledDelivery
    } else {
        RelayForfeitArm::ZeroDelivery
    }
}

fn undelivered_response_suffix_exists(evidence: WatcherRelayLivenessEvidence<'_>) -> bool {
    !evidence.full_response.trim().is_empty()
        && evidence.response_sent_offset < evidence.full_response.len()
        && evidence
            .full_response
            .is_char_boundary(evidence.response_sent_offset)
}

pub(super) fn fresh_watcher_heartbeat_blocks_rebind(
    evidence: WatcherRelayLivenessEvidence<'_>,
    relay_liveness_forfeited: bool,
) -> bool {
    capture_progress_recent(evidence)
        || (undelivered_response_suffix_exists(evidence) && !relay_liveness_forfeited)
}

pub(super) fn relay_liveness_forfeited(evidence: WatcherRelayLivenessEvidence<'_>) -> bool {
    if !undelivered_response_suffix_exists(evidence) {
        return false;
    }

    let zero_delivery_forfeited = relay_forfeit_arm(evidence.prior_delivery_evidence)
        == RelayForfeitArm::ZeroDelivery
        && evidence.response_sent_offset == 0
        && evidence.last_watcher_relayed_offset.is_none()
        && !evidence.terminal_delivery_committed
        && evidence.turn_age_secs.is_some_and(|age| {
            (RELAY_FORFEIT_MIN_AGE_SECS..=MAX_RELAY_LIVENESS_TURN_AGE_SECS).contains(&age)
        });

    // The complementary prior-delivery arm still requires a valid watcher clock
    // and stalled consumer frontier before destructive recovery may proceed.
    // Legacy/rebind rows without a relay timestamp and rows without a parseable
    // turn age conservatively abstain. A large forward wall-clock jump can inflate
    // both ages together; #5007 activation must add a monotonic-clock safeguard.
    let stalled_relay_forfeited = relay_forfeit_arm(evidence.prior_delivery_evidence)
        == RelayForfeitArm::StalledDelivery
        && match (
            valid_elapsed_secs(evidence.last_watcher_relayed_at_unix, evidence.now_unix),
            evidence.turn_age_secs,
        ) {
            (Some(stall_age_secs), Some(turn_age_secs)) => {
                relay_frontier_stalled(
                    evidence.relay_frontier_at_snapshot,
                    evidence.relay_frontier_now,
                ) && capture_exceeds_observed_frontier(
                    evidence.output_len_now,
                    evidence.relay_frontier_now,
                ) && stall_age_secs >= RELAY_FORFEIT_MIN_AGE_SECS
                    && stall_age_secs <= turn_age_secs
                    && turn_age_secs <= MAX_RELAY_LIVENESS_TURN_AGE_SECS
            }
            _ => false,
        };

    zero_delivery_forfeited || stalled_relay_forfeited
}

fn valid_elapsed_secs(timestamp: Option<i64>, now_unix: i64) -> Option<i64> {
    let timestamp = timestamp?;
    (timestamp >= 0 && timestamp <= now_unix).then(|| now_unix - timestamp)
}

fn relay_frontier_stalled(previous: Option<u64>, current: Option<u64>) -> bool {
    matches!((previous, current), (Some(previous), Some(current)) if current == previous)
}

fn capture_exceeds_observed_frontier(
    output_len_now: Option<u64>,
    relay_frontier_now: Option<u64>,
) -> bool {
    output_len_now
        .zip(relay_frontier_now)
        .is_some_and(|(output_len, relay_frontier)| output_len > relay_frontier)
}

fn capture_progress_recent(evidence: WatcherRelayLivenessEvidence<'_>) -> bool {
    matches!(
        (evidence.output_len_at_snapshot, evidence.output_len_now),
        (Some(previous), Some(current)) if current > previous
    ) || relay_frontier_advanced(
        evidence.relay_frontier_at_snapshot,
        evidence.relay_frontier_now,
    ) || evidence.output_mtime_age_secs.is_some_and(|age_secs| {
        age_secs < crate::services::tui_turn_state::STALE_USER_SUBMITTED_RECLAIM_SECS
    })
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
            turn_age_secs: Some(RELAY_FORFEIT_MIN_AGE_SECS + 1),
            now_unix: 100_000,
        }
    }

    #[test]
    fn destructive_cancel_recent_capture_growth_blocks_rebind() {
        assert!(relay_liveness_forfeited(evidence()));
        assert!(fresh_watcher_heartbeat_blocks_rebind(
            evidence(),
            relay_liveness_forfeited(evidence()),
        ));
    }

    #[test]
    fn destructive_cancel_fresh_capture_mtime_blocks_rebind() {
        let fresh = WatcherRelayLivenessEvidence {
            output_len_now: evidence().output_len_at_snapshot,
            relay_frontier_now: evidence().relay_frontier_at_snapshot,
            ..evidence()
        };
        assert!(fresh_watcher_heartbeat_blocks_rebind(
            fresh,
            relay_liveness_forfeited(fresh),
        ));
    }

    #[test]
    fn destructive_cancel_consumer_progress_still_blocks_rebind() {
        let advanced = WatcherRelayLivenessEvidence {
            relay_frontier_now: Some(6_281_997),
            output_len_now: Some(512_146),
            output_mtime_age_secs: Some(601),
            ..evidence()
        };
        assert!(fresh_watcher_heartbeat_blocks_rebind(
            advanced,
            relay_liveness_forfeited(advanced),
        ));

        let first_advance = WatcherRelayLivenessEvidence {
            relay_frontier_at_snapshot: None,
            relay_frontier_now: Some(1),
            ..advanced
        };
        assert!(fresh_watcher_heartbeat_blocks_rebind(
            first_advance,
            relay_liveness_forfeited(first_advance),
        ));
    }

    #[test]
    fn destructive_cancel_minimum_age_preserves_grace_window() {
        let within_grace = WatcherRelayLivenessEvidence {
            turn_age_secs: Some(RELAY_FORFEIT_MIN_AGE_SECS - 1),
            ..evidence()
        };
        assert!(fresh_watcher_heartbeat_blocks_rebind(
            within_grace,
            relay_liveness_forfeited(within_grace),
        ));
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
        assert!(fresh_watcher_heartbeat_blocks_rebind(
            no_divergence,
            relay_liveness_forfeited(no_divergence),
        ));
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
        assert!(fresh_watcher_heartbeat_blocks_rebind(
            legacy,
            relay_liveness_forfeited(legacy),
        ));
    }

    #[test]
    fn destructive_cancel_partial_delivery_stalled_arm_is_reachable() {
        let full_response = "delivered prefix and undelivered suffix";
        let response_sent_offset = "delivered prefix".len();
        assert!(full_response.is_char_boundary(response_sent_offset));
        let stalled = WatcherRelayLivenessEvidence {
            last_watcher_relayed_offset: Some(6_281_996),
            last_watcher_relayed_at_unix: Some(90_000),
            output_len_at_snapshot: Some(6_282_100),
            output_len_now: Some(6_282_100),
            output_mtime_age_secs: Some(601),
            relay_frontier_at_snapshot: Some(6_281_996),
            relay_frontier_now: Some(6_281_996),
            full_response,
            response_sent_offset,
            prior_delivery_evidence: true,
            turn_age_secs: Some(20_000),
            now_unix: 100_000,
            ..evidence()
        };
        assert_eq!(
            relay_forfeit_arm(stalled.prior_delivery_evidence),
            RelayForfeitArm::StalledDelivery
        );
        assert!(relay_liveness_forfeited(stalled));
        assert!(!fresh_watcher_heartbeat_blocks_rebind(
            stalled,
            relay_liveness_forfeited(stalled),
        ));
    }

    #[test]
    fn destructive_cancel_rewound_offset_with_prior_delivery_uses_stalled_arm() {
        let rewound = WatcherRelayLivenessEvidence {
            last_watcher_relayed_at_unix: Some(90_000),
            output_len_at_snapshot: Some(6_282_100),
            output_len_now: Some(6_282_100),
            output_mtime_age_secs: Some(601),
            relay_frontier_at_snapshot: Some(6_281_996),
            relay_frontier_now: Some(6_281_996),
            prior_delivery_evidence: true,
            turn_age_secs: Some(20_000),
            now_unix: 100_000,
            ..evidence()
        };
        assert_eq!(
            relay_forfeit_arm(rewound.prior_delivery_evidence),
            RelayForfeitArm::StalledDelivery
        );
        assert!(relay_liveness_forfeited(rewound));
        assert!(!fresh_watcher_heartbeat_blocks_rebind(
            rewound,
            relay_liveness_forfeited(rewound),
        ));
    }

    #[test]
    fn destructive_cancel_relay_timestamp_cannot_predate_turn() {
        let clock_failure_stamp = WatcherRelayLivenessEvidence {
            last_watcher_relayed_at_unix: Some(0),
            output_len_now: Some(6_282_100),
            relay_frontier_at_snapshot: Some(6_281_996),
            relay_frontier_now: Some(6_281_996),
            prior_delivery_evidence: true,
            turn_age_secs: Some(3_600),
            now_unix: 1_800_000_000,
            ..evidence()
        };
        assert!(!relay_liveness_forfeited(clock_failure_stamp));
        assert!(fresh_watcher_heartbeat_blocks_rebind(
            clock_failure_stamp,
            relay_liveness_forfeited(clock_failure_stamp),
        ));
    }

    #[test]
    fn destructive_cancel_without_unsent_payload_preserves_halted_recovery() {
        let no_payload = WatcherRelayLivenessEvidence {
            output_len_now: evidence().output_len_at_snapshot,
            output_mtime_age_secs: Some(601),
            full_response: "",
            ..evidence()
        };
        assert!(!fresh_watcher_heartbeat_blocks_rebind(no_payload, false));
    }

    #[test]
    fn destructive_cancel_legacy_epoch_turn_age_abstains() {
        let legacy_epoch = WatcherRelayLivenessEvidence {
            last_watcher_relayed_offset: Some(6_281_996),
            last_watcher_relayed_at_unix: Some(90_000),
            output_len_now: Some(6_282_100),
            relay_frontier_at_snapshot: Some(6_281_996),
            relay_frontier_now: Some(6_281_996),
            now_unix: 100_000,
            turn_age_secs: Some(MAX_RELAY_LIVENESS_TURN_AGE_SECS + 1),
            prior_delivery_evidence: true,
            ..evidence()
        };
        assert!(!relay_liveness_forfeited(legacy_epoch));
        assert!(fresh_watcher_heartbeat_blocks_rebind(
            legacy_epoch,
            relay_liveness_forfeited(legacy_epoch),
        ));
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
            assert!(fresh_watcher_heartbeat_blocks_rebind(
                invalid_timestamp,
                relay_liveness_forfeited(invalid_timestamp),
            ));
        }
    }

    #[test]
    fn destructive_cancel_tool_only_turn_abstains_from_payload_forfeit() {
        let tool_only = WatcherRelayLivenessEvidence {
            output_len_now: evidence().output_len_at_snapshot,
            output_mtime_age_secs: Some(601),
            full_response: "",
            ..evidence()
        };
        assert!(!relay_liveness_forfeited(tool_only));
        assert!(!fresh_watcher_heartbeat_blocks_rebind(tool_only, false));
    }
}
