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
enum EpisodeEvidence {
    Matched,
    Mismatched,
    OperandAbsent,
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

    // #5942 r2: `Expired` answers exactly what `Unknown` answers, and that is
    // the point. The channels this variant is reachable for read
    // `Unknown{transcript_unresolved}` before the TTL fires, so giving
    // `Expired` a different rule would mean the warrant silently changed the
    // moment a timer elapsed — a behaviour change #5942 never asked for in
    // either direction.
    //
    // r1 answered `Deny` here on the argument that an expired ledger is less
    // evidence than an unknown one. The r2 review was right that this is worse,
    // not safer: an expired ledger is PERMANENTLY expired, so `Deny` was a
    // permanent trap with no escape, and it removed the episode-gated
    // `ClearStaleThreadProof`/`ClearOrphanPendingToken` cleanup that abandoned
    // routine channels are exactly the population for.
    //
    // r3 (P2-1) corrects what r2 claimed for `RequireEpisode`. It is NOT a
    // barrier for this population: `exact_episode_evidence` answers
    // `OperandAbsent` when either nonce is missing, and the match below treats
    // `Matched | OperandAbsent` alike, so an abandoned routine channel — which
    // by construction has no live mailbox or inflight episode — passes the
    // warrant. The rule only bites on `Mismatched`, i.e. two nonces that
    // disagree. The decision stands anyway, for the reason above and not for
    // the reason r2 gave: the point is that `Expired` grants no MORE than the
    // `Unknown` the same channel carried one tick earlier. `Expired` changes
    // one thing only — it withdraws a health vote — and the warrant surface is
    // deliberately where it changes nothing.
    match (action, verdict) {
        (ClearStaleThreadProof, Reachable) => PassLedger,
        (ClearStaleThreadProof, Degraded { .. }) => RequireEpisode,
        (ClearStaleThreadProof, TransportUnknown { .. }) => Deny,
        (ClearStaleThreadProof, Unreachable { .. }) => RequireEpisode,
        (ClearStaleThreadProof, Unknown { .. }) => RequireEpisode,
        (ClearStaleThreadProof, Expired { .. }) => RequireEpisode,
        (ClearOrphanPendingToken, Reachable) => PassLedger,
        (ClearOrphanPendingToken, Degraded { .. }) => RequireEpisode,
        (ClearOrphanPendingToken, TransportUnknown { .. }) => Deny,
        (ClearOrphanPendingToken, Unreachable { .. }) => RequireEpisode,
        (ClearOrphanPendingToken, Unknown { .. }) => RequireEpisode,
        (ClearOrphanPendingToken, Expired { .. }) => RequireEpisode,
        (ReattachWatcher, Reachable) => PassLedger,
        (ReattachWatcher, Degraded { .. }) => RequireEpisode,
        (ReattachWatcher, TransportUnknown { .. }) if pinned_adoption => PassLedger,
        (ReattachWatcher, TransportUnknown { .. }) => Deny,
        (ReattachWatcher, Unreachable { .. }) => RequireEpisode,
        (ReattachWatcher, Unknown { .. }) => RequireEpisode,
        (ReattachWatcher, Expired { .. }) => RequireEpisode,
        (DrainPendingQueue, Reachable) => PassLedger,
        (DrainPendingQueue, Degraded { .. }) => RequireEpisode,
        (DrainPendingQueue, TransportUnknown { .. }) => Deny,
        (DrainPendingQueue, Unreachable { .. }) => RequireEpisode,
        (DrainPendingQueue, Unknown { .. }) => RequireEpisode,
        (DrainPendingQueue, Expired { .. }) => RequireEpisode,
        (ObserveOnly, _) => Deny,
    }
}

fn compare_episode_nonces(
    mailbox_nonce: Option<&str>,
    inflight_nonce: Option<&str>,
) -> EpisodeEvidence {
    let Some(mailbox_nonce) = mailbox_nonce.filter(|nonce| !nonce.is_empty()) else {
        return EpisodeEvidence::OperandAbsent;
    };
    let Some(inflight_nonce) = inflight_nonce.filter(|nonce| !nonce.is_empty()) else {
        return EpisodeEvidence::OperandAbsent;
    };
    if inflight_nonce == mailbox_nonce {
        EpisodeEvidence::Matched
    } else {
        EpisodeEvidence::Mismatched
    }
}

fn exact_episode_evidence(
    provider: &ProviderKind,
    snapshot: &health::WatcherStateSnapshot,
) -> EpisodeEvidence {
    let inflight = super::super::inflight::load_inflight_state_read_only(
        provider,
        snapshot.relay_health.channel_id,
    );
    compare_episode_nonces(
        snapshot.mailbox_active_turn_nonce.as_deref(),
        inflight
            .as_ref()
            .and_then(|state| state.turn_nonce.as_deref()),
    )
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
        WarrantRule::RequireEpisode => match exact_episode_evidence(provider, snapshot) {
            EpisodeEvidence::Matched | EpisodeEvidence::OperandAbsent => DestructiveWarrant {
                eligible: true,
                skipped_reason: None,
            },
            EpisodeEvidence::Mismatched => DestructiveWarrant {
                eligible: false,
                skipped_reason: Some("axis_b_exact_episode_required"),
            },
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
            // #5942 r2 (P1-1): without this row the truth table below ran over
            // five of six variants, and flipping the four `Expired` arms to
            // `PassLedger` — turning an expiry into a destruction warrant —
            // left the whole suite green. `match` exhaustiveness forced the
            // arms to be WRITTEN; only the fixture pins what they SAY.
            ReachabilityVerdict::Expired {
                unobserved_for_secs: 1,
            },
        ]
    }

    /// The fixture above is a hand-written list, and a hand-written list is
    /// exactly what a new variant walks straight past. This index has no `_`
    /// arm, so a seventh `ReachabilityVerdict` stops this module compiling
    /// until someone names it — the same device
    /// `reachability::verdict::tests::unknown_reason_index` uses, borrowed here
    /// because this is the second place a missing row silently narrowed a
    /// table.
    fn verdict_index(verdict: &ReachabilityVerdict) -> usize {
        match verdict {
            ReachabilityVerdict::Reachable => 0,
            ReachabilityVerdict::Degraded { .. } => 1,
            ReachabilityVerdict::TransportUnknown { .. } => 2,
            ReachabilityVerdict::Unreachable { .. } => 3,
            ReachabilityVerdict::Unknown { .. } => 4,
            ReachabilityVerdict::Expired { .. } => 5,
        }
    }

    /// Every variant appears in `verdicts()` exactly once, proven the way
    /// `every_unknown_reason_is_named_exactly_once` proves its table: the
    /// indices the fixture yields must cover every slot without collision,
    /// which they can only do if the fixture is a permutation of the arms
    /// above.
    #[test]
    fn the_warrant_fixture_covers_every_verdict_variant_exactly_once() {
        const VERDICT_COUNT: usize = 6;
        let mut claimed = [false; VERDICT_COUNT];
        for verdict in verdicts() {
            let slot = &mut claimed[verdict_index(&verdict)];
            assert!(
                !*slot,
                "{verdict:?} claims an index the fixture already used"
            );
            *slot = true;
        }
        for (index, claimed) in claimed.iter().enumerate() {
            assert!(*claimed, "no fixture row claims verdict index {index}");
        }
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
            // #5942 r2: `Expired` is warrant-neutral — the same rule the same
            // channel had one tick earlier as `Unknown`.
            WarrantRule::RequireEpisode,
        ];
        assert_eq!(
            verdicts().len(),
            expected.len(),
            "the fixture and the expectation must stay the same length, or this \
             table silently checks a prefix"
        );
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
    fn clear_orphan_pending_token_requires_the_same_episode_when_both_operands_exist() {
        let provider = ProviderKind::Codex;
        let mut snapshot = super::super::axis_b_tests::quiet_snapshot_for_warrant_tests(54_642);
        snapshot.reachability_observation = Some((
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::NeverObserved, 1),
            1,
        ));
        snapshot.mailbox_active_turn_nonce = Some("mailbox-episode".to_string());
        let root = tempfile::tempdir().expect("inflight root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
        let mut state = super::super::inflight::InflightTurnState::new(
            provider.clone(),
            snapshot.relay_health.channel_id,
            None,
            1,
            2,
            3,
            "episode witness".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        state.turn_nonce = Some("different-episode".to_string());
        super::super::inflight::save_inflight_state(&state).expect("save mismatch row");
        let mismatched = destructive_warrant_bind(
            true,
            RelayRecoveryActionKind::ClearOrphanPendingToken,
            &provider,
            Some(&snapshot),
            false,
        );
        assert!(!mismatched.eligible);
        assert_eq!(
            mismatched.skipped_reason,
            Some("axis_b_exact_episode_required")
        );

        state.turn_nonce = Some("mailbox-episode".to_string());
        super::super::inflight::clear_inflight_state(&provider, snapshot.relay_health.channel_id);
        super::super::inflight::save_inflight_state(&state).expect("save matching row");
        assert!(
            destructive_warrant_bind(
                true,
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                &provider,
                Some(&snapshot),
                false,
            )
            .eligible
        );
    }

    #[test]
    fn exact_episode_distinguishes_match_mismatch_and_missing_operands() {
        assert_eq!(
            compare_episode_nonces(Some("episode-a"), Some("episode-a")),
            EpisodeEvidence::Matched
        );
        assert_eq!(
            compare_episode_nonces(Some("episode-a"), Some("episode-b")),
            EpisodeEvidence::Mismatched
        );
        for (mailbox, inflight) in [
            (None, Some("episode-a")),
            (Some("episode-a"), None),
            (Some(""), Some("episode-a")),
            (Some("episode-a"), Some("")),
        ] {
            assert_eq!(
                compare_episode_nonces(mailbox, inflight),
                EpisodeEvidence::OperandAbsent
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

        let mut snapshot = super::super::axis_b_tests::quiet_snapshot_for_warrant_tests(54_643);
        assert!(snapshot.reachability_observation().is_none());
        assert!(
            destructive_warrant_bind(
                true,
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                &provider,
                Some(&snapshot),
                false,
            )
            .eligible,
            "missing reachability operand must preserve structural eligibility"
        );

        snapshot.reachability_observation = Some((
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::NeverObserved, 1),
            1,
        ));
        assert!(snapshot.mailbox_active_turn_nonce.is_none());
        assert!(
            destructive_warrant_bind(
                true,
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                &provider,
                Some(&snapshot),
                false,
            )
            .eligible,
            "legacy missing episode nonce must abstain rather than veto"
        );
    }
}
