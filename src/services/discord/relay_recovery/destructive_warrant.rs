//! Planning-time axis-B veto for automatic destructive recovery candidates.
use super::{RelayRecoveryActionKind, health};
use crate::services::provider::ProviderKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WarrantRule {
    PassLedger,
    RequireEpisode,
    Deny,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DestructiveWarrant {
    pub(crate) eligible: bool,
    pub(crate) skipped_reason: Option<&'static str>,
}

fn rule(
    action: RelayRecoveryActionKind,
    verdict: &health::reachability::verdict::ReachabilityVerdict,
    pinned_adoption: bool,
) -> WarrantRule {
    use RelayRecoveryActionKind::*;
    use WarrantRule::*;
    use health::reachability::verdict::ReachabilityVerdict::*;

    match (action, verdict) {
        (ClearStaleThreadProof, Reachable) => PassLedger,
        (ClearStaleThreadProof, Degraded { .. }) => RequireEpisode,
        (ClearStaleThreadProof, TransportUnknown { .. }) => Deny,
        (ClearStaleThreadProof, Unreachable { .. }) => RequireEpisode,
        (ClearStaleThreadProof, Unknown { .. }) => RequireEpisode,
        (ClearOrphanPendingToken, Reachable) => PassLedger,
        (ClearOrphanPendingToken, Degraded { .. }) => RequireEpisode,
        (ClearOrphanPendingToken, TransportUnknown { .. }) => Deny,
        (ClearOrphanPendingToken, Unreachable { .. }) => RequireEpisode,
        (ClearOrphanPendingToken, Unknown { .. }) => RequireEpisode,
        (ReattachWatcher, Reachable) => PassLedger,
        (ReattachWatcher, Degraded { .. }) => RequireEpisode,
        (ReattachWatcher, TransportUnknown { .. }) if pinned_adoption => PassLedger,
        (ReattachWatcher, TransportUnknown { .. }) => Deny,
        (ReattachWatcher, Unreachable { .. }) => RequireEpisode,
        (ReattachWatcher, Unknown { .. }) => RequireEpisode,
        (DrainPendingQueue, Reachable) => PassLedger,
        (DrainPendingQueue, Degraded { .. }) => RequireEpisode,
        (DrainPendingQueue, TransportUnknown { .. }) => Deny,
        (DrainPendingQueue, Unreachable { .. }) => RequireEpisode,
        (DrainPendingQueue, Unknown { .. }) => RequireEpisode,
        (ObserveOnly | ReportRelayUnreachable, _) => Deny,
    }
}

fn exact_episode_matches(
    action: RelayRecoveryActionKind,
    provider: &ProviderKind,
    snapshot: &health::WatcherStateSnapshot,
) -> bool {
    let Some(mailbox_nonce) = snapshot
        .mailbox_active_turn_nonce
        .as_deref()
        .filter(|nonce| !nonce.is_empty())
    else {
        return false;
    };
    if action == RelayRecoveryActionKind::ClearOrphanPendingToken {
        return true;
    }
    super::inflight::load_inflight_state_read_only(provider, snapshot.relay_health.channel_id)
        .and_then(|state| state.turn_nonce)
        .as_deref()
        .is_some_and(|inflight_nonce| !inflight_nonce.is_empty() && inflight_nonce == mailbox_nonce)
}

pub(in crate::services::discord) const fn structural_candidate_apply(eligible: bool) -> bool {
    eligible
}

pub(in crate::services::discord) fn destructive_warrant_bind(
    structural_eligible: bool,
    action: RelayRecoveryActionKind,
    provider: &ProviderKind,
    snapshot: Option<&health::WatcherStateSnapshot>,
    pinned_adoption: bool,
) -> DestructiveWarrant {
    if !structural_eligible {
        return DestructiveWarrant {
            eligible: false,
            skipped_reason: None,
        };
    }
    let Some(snapshot) = snapshot else {
        return DestructiveWarrant {
            eligible: true,
            skipped_reason: None,
        };
    };
    let Some((verdict, _)) = snapshot.reachability_observation() else {
        return DestructiveWarrant {
            eligible: true,
            skipped_reason: None,
        };
    };
    match rule(action, verdict, pinned_adoption) {
        WarrantRule::PassLedger => DestructiveWarrant {
            eligible: true,
            skipped_reason: None,
        },
        WarrantRule::RequireEpisode if exact_episode_matches(action, provider, snapshot) => {
            DestructiveWarrant {
                eligible: true,
                skipped_reason: None,
            }
        }
        WarrantRule::RequireEpisode => DestructiveWarrant {
            eligible: false,
            skipped_reason: Some("axis_b_exact_episode_required"),
        },
        WarrantRule::Deny => DestructiveWarrant {
            eligible: false,
            skipped_reason: Some("axis_b_transport_trace_live"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use health::reachability::verdict::{
        ReachabilityUnknownReason, ReachabilityVerdict, TransportUnknownEvidence,
    };

    fn verdicts() -> Vec<ReachabilityVerdict> {
        vec![
            ReachabilityVerdict::Reachable,
            ReachabilityVerdict::Degraded {
                oldest_unsatisfied_age_secs: 1,
                uncovered_ranges: 1,
            },
            ReachabilityVerdict::TransportUnknown {
                since_secs: 1,
                evidence: TransportUnknownEvidence::UnreleasedDeliveryLease,
            },
            ReachabilityVerdict::Unreachable {
                oldest_unsatisfied_age_secs: 1,
                uncovered_ranges: 1,
            },
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::NeverObserved, 1),
        ]
    }

    const ACTIONS: [RelayRecoveryActionKind; 4] = [
        RelayRecoveryActionKind::ClearStaleThreadProof,
        RelayRecoveryActionKind::ClearOrphanPendingToken,
        RelayRecoveryActionKind::ReattachWatcher,
        RelayRecoveryActionKind::DrainPendingQueue,
    ];

    #[test]
    fn truth_table_is_explicit_for_every_destructive_action_and_verdict() {
        let expected = [
            WarrantRule::PassLedger,
            WarrantRule::RequireEpisode,
            WarrantRule::Deny,
            WarrantRule::RequireEpisode,
            WarrantRule::RequireEpisode,
        ];
        for action in ACTIONS {
            assert_eq!(
                verdicts()
                    .iter()
                    .map(|v| rule(action, v, false))
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn pinned_adoption_only_relaxes_transport_unknown_reattach() {
        for verdict in verdicts() {
            let destructive = rule(RelayRecoveryActionKind::ReattachWatcher, &verdict, false);
            let adoption = rule(RelayRecoveryActionKind::ReattachWatcher, &verdict, true);
            assert_eq!(
                adoption,
                if matches!(verdict, ReachabilityVerdict::TransportUnknown { .. }) {
                    WarrantRule::PassLedger
                } else {
                    destructive
                }
            );
        }
    }

    #[test]
    fn warrant_is_monotone_and_abstains_when_its_operand_is_absent() {
        let provider = ProviderKind::Codex;
        for action in ACTIONS {
            assert!(!destructive_warrant_bind(false, action, &provider, None, false).eligible);
            assert!(destructive_warrant_bind(true, action, &provider, None, false).eligible);
        }
    }
}
