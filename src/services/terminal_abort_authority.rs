//! Dormant, pure authority model for terminal-abort decisions (#4895 Slice 1).
//!
//! This module performs no I/O and has no production call sites. It cannot kill
//! tmux sessions, mutate dispatch state, clear authentication state, or settle a
//! turn. Later adapters must preserve every exact-coordinate and typed-outcome
//! fence before this authority can be activated.

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TerminalCoordinate {
    channel_id: u64,
    turn_id: String,
    dispatch_id: String,
    generation: u64,
}

impl TerminalCoordinate {
    pub(crate) fn new(
        channel_id: u64,
        turn_id: impl Into<String>,
        dispatch_id: impl Into<String>,
        generation: u64,
    ) -> Option<Self> {
        let coordinate = Self {
            channel_id,
            turn_id: turn_id.into(),
            dispatch_id: dispatch_id.into(),
            generation,
        };
        coordinate.is_complete().then_some(coordinate)
    }

    fn is_complete(&self) -> bool {
        self.channel_id != 0
            && !self.turn_id.is_empty()
            && !self.dispatch_id.is_empty()
            && self.generation != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TmuxSessionId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalReason {
    PromptTooLong,
    Authentication,
    Provider(ProviderErrorStatus),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalAuthority {
    coordinate: TerminalCoordinate,
    kill_target: TmuxSessionId,
    reason: TerminalReason,
}

impl TerminalAuthority {
    pub(crate) fn new(
        coordinate: TerminalCoordinate,
        kill_target: TmuxSessionId,
        reason: TerminalReason,
    ) -> Option<Self> {
        (coordinate.is_complete() && kill_target.0 != 0).then_some(Self {
            coordinate,
            kill_target,
            reason,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClosedNotice {
    MissingExactTerminalTuple,
    InvalidExactTerminalTuple,
    NewerDispatchPending,
    CoordinateMismatch,
    ClaimReplay,
    ConflictingClaim,
    KillNotConfirmed,
    KillTargetMismatch,
    AuthClearPending,
    AuthOutcomeStale,
    AuthAlreadyCleared,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NoRowEvidence {
    Legacy,
    Incomplete,
    Exact(TerminalAuthority),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NoRowAction {
    Authorize(TerminalAuthority),
    ObserveOnly(ClosedNotice),
}

/// Classify a missing-row observation without reconstructing authority from
/// ambient watcher state. A newer pending dispatch always fences the observation.
pub(crate) fn classify_no_row_action(
    evidence: NoRowEvidence,
    pending_dispatch: Option<&TerminalCoordinate>,
) -> NoRowAction {
    let authority = match evidence {
        NoRowEvidence::Legacy => {
            return NoRowAction::ObserveOnly(ClosedNotice::MissingExactTerminalTuple);
        }
        NoRowEvidence::Incomplete => {
            return NoRowAction::ObserveOnly(ClosedNotice::InvalidExactTerminalTuple);
        }
        NoRowEvidence::Exact(authority) => authority,
    };

    if !authority.coordinate.is_complete() || authority.kill_target.0 == 0 {
        return NoRowAction::ObserveOnly(ClosedNotice::InvalidExactTerminalTuple);
    }
    if pending_dispatch.is_some_and(|pending| pending != &authority.coordinate) {
        return NoRowAction::ObserveOnly(ClosedNotice::NewerDispatchPending);
    }
    NoRowAction::Authorize(authority)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DispatchClaim {
    coordinate: TerminalCoordinate,
    claim_id: String,
}

impl DispatchClaim {
    pub(crate) fn new(coordinate: TerminalCoordinate, claim_id: impl Into<String>) -> Option<Self> {
        let claim_id = claim_id.into();
        (coordinate.is_complete() && !claim_id.is_empty()).then_some(Self {
            coordinate,
            claim_id,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DispatchAdmission {
    Admitted(DispatchClaim),
    ObserveOnly(ClosedNotice),
}

/// Admit one exact-coordinate claim. Replays and conflicting existing claims are
/// observations only and confer no dispatch mutation authority.
pub(crate) fn admit_dispatch_claim(
    authority: &TerminalAuthority,
    existing: Option<&DispatchClaim>,
    requested: DispatchClaim,
) -> DispatchAdmission {
    if requested.coordinate != authority.coordinate {
        return DispatchAdmission::ObserveOnly(ClosedNotice::CoordinateMismatch);
    }
    match existing {
        Some(current) if current == &requested => {
            DispatchAdmission::ObserveOnly(ClosedNotice::ClaimReplay)
        }
        Some(_) => DispatchAdmission::ObserveOnly(ClosedNotice::ConflictingClaim),
        None => DispatchAdmission::Admitted(requested),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KillOutcome {
    Killed {
        session_id: TmuxSessionId,
    },
    PreservedIdentityChanged {
        expected: TmuxSessionId,
        observed: TmuxSessionId,
    },
    CaptureFailed,
    TimedOut,
    CommandFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KillNotice {
    Killed,
    Preserved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettlementPermission {
    Allowed,
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KillReduction {
    pub notice: KillNotice,
    pub settlement: SettlementPermission,
    pub closed_notice: Option<ClosedNotice>,
}

/// Reduce an adapter-reported kill result. Only a confirmed kill of the immutable
/// session ID may produce a killed notice or unlock kill-dependent settlement.
pub(crate) fn reduce_kill_result(
    authority: &TerminalAuthority,
    outcome: KillOutcome,
) -> KillReduction {
    match outcome {
        KillOutcome::Killed { session_id } if session_id == authority.kill_target => {
            KillReduction {
                notice: KillNotice::Killed,
                settlement: SettlementPermission::Allowed,
                closed_notice: None,
            }
        }
        KillOutcome::Killed { .. } => KillReduction {
            notice: KillNotice::Preserved,
            settlement: SettlementPermission::Blocked,
            closed_notice: Some(ClosedNotice::KillTargetMismatch),
        },
        KillOutcome::PreservedIdentityChanged { .. }
        | KillOutcome::CaptureFailed
        | KillOutcome::TimedOut
        | KillOutcome::CommandFailed => KillReduction {
            notice: KillNotice::Preserved,
            settlement: SettlementPermission::Blocked,
            closed_notice: Some(ClosedNotice::KillNotConfirmed),
        },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthClearAttempt {
    coordinate: TerminalCoordinate,
    attempt: u32,
}

impl AuthClearAttempt {
    fn new(coordinate: TerminalCoordinate, attempt: u32) -> Option<Self> {
        (coordinate.is_complete() && attempt != 0).then_some(Self {
            coordinate,
            attempt,
        })
    }

    fn next(&self) -> Self {
        Self {
            coordinate: self.coordinate.clone(),
            attempt: self.attempt.checked_add(1).unwrap_or(self.attempt),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AuthClearOutcome {
    Cleared(AuthClearAttempt),
    PersistFailed(AuthClearAttempt),
    Stale(AuthClearAttempt),
}

impl AuthClearOutcome {
    pub(crate) fn cleared(coordinate: TerminalCoordinate, attempt: u32) -> Option<Self> {
        AuthClearAttempt::new(coordinate, attempt).map(Self::Cleared)
    }

    pub(crate) fn persist_failed(coordinate: TerminalCoordinate, attempt: u32) -> Option<Self> {
        AuthClearAttempt::new(coordinate, attempt).map(Self::PersistFailed)
    }

    pub(crate) fn stale(coordinate: TerminalCoordinate, attempt: u32) -> Option<Self> {
        AuthClearAttempt::new(coordinate, attempt).map(Self::Stale)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AuthClearObligation {
    NotRequired,
    Pending(AuthClearAttempt),
    Satisfied(AuthClearAttempt),
}

impl AuthClearObligation {
    pub(crate) fn pending(authority: &TerminalAuthority) -> Self {
        Self::Pending(
            AuthClearAttempt::new(authority.coordinate.clone(), 1)
                .expect("terminal authority always contains a complete coordinate"),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AuthRetryObligation {
    None,
    Pending(AuthClearAttempt),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthClearReduction {
    pub obligation: AuthClearObligation,
    pub retry: AuthRetryObligation,
    pub settlement: SettlementPermission,
    pub closed_notice: Option<ClosedNotice>,
}

/// Advance the durable auth-clear obligation by one exact attempt. Failed clear
/// persistence and stale clear outcomes retain a pending retry and block
/// settlement. A matching success consumes the obligation exactly once.
pub(crate) fn reduce_auth_clear_obligation(
    current: &AuthClearObligation,
    outcome: AuthClearOutcome,
) -> AuthClearReduction {
    let attempted = match outcome {
        AuthClearOutcome::Cleared(attempt)
        | AuthClearOutcome::PersistFailed(attempt)
        | AuthClearOutcome::Stale(attempt) => attempt,
    };

    let AuthClearObligation::Pending(expected) = current else {
        return match current {
            AuthClearObligation::NotRequired => AuthClearReduction {
                obligation: AuthClearObligation::NotRequired,
                retry: AuthRetryObligation::None,
                settlement: SettlementPermission::Allowed,
                closed_notice: None,
            },
            AuthClearObligation::Satisfied(cleared) => AuthClearReduction {
                obligation: AuthClearObligation::Satisfied(cleared.clone()),
                retry: AuthRetryObligation::None,
                settlement: SettlementPermission::Allowed,
                closed_notice: Some(ClosedNotice::AuthAlreadyCleared),
            },
            AuthClearObligation::Pending(_) => unreachable!(),
        };
    };

    if &attempted != expected {
        return AuthClearReduction {
            obligation: current.clone(),
            retry: AuthRetryObligation::Pending(expected.clone()),
            settlement: SettlementPermission::Blocked,
            closed_notice: Some(ClosedNotice::AuthOutcomeStale),
        };
    }

    match outcome {
        AuthClearOutcome::Cleared(_) => AuthClearReduction {
            obligation: AuthClearObligation::Satisfied(attempted),
            retry: AuthRetryObligation::None,
            settlement: SettlementPermission::Allowed,
            closed_notice: None,
        },
        AuthClearOutcome::PersistFailed(_) | AuthClearOutcome::Stale(_) => {
            let retry = attempted.next();
            AuthClearReduction {
                obligation: AuthClearObligation::Pending(retry.clone()),
                retry: AuthRetryObligation::Pending(retry),
                settlement: SettlementPermission::Blocked,
                closed_notice: Some(ClosedNotice::AuthClearPending),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderErrorCode {
    RateLimitExceeded,
    Overloaded,
    AuthenticationFailed,
    PromptTooLong,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderErrorStatus {
    Http429,
    Http529,
    Code(ProviderErrorCode),
}

/// Parse only the producer-owned closed envelope. Exact byte equality is
/// intentional: token scans, prose, quoting, nesting, partial fields, duplicate
/// fields, and ambiguous combinations do not carry terminal authority.
pub(crate) fn parse_closed_provider_status(input: &str) -> Option<ProviderErrorStatus> {
    match input {
        "[API Error: status=429]" => Some(ProviderErrorStatus::Http429),
        "[API Error: status=529]" => Some(ProviderErrorStatus::Http529),
        "[API Error: code=rate_limit_exceeded]" => Some(ProviderErrorStatus::Code(
            ProviderErrorCode::RateLimitExceeded,
        )),
        "[API Error: code=overloaded]" => {
            Some(ProviderErrorStatus::Code(ProviderErrorCode::Overloaded))
        }
        "[API Error: code=authentication_failed]" => Some(ProviderErrorStatus::Code(
            ProviderErrorCode::AuthenticationFailed,
        )),
        "[API Error: code=prompt_too_long]" => {
            Some(ProviderErrorStatus::Code(ProviderErrorCode::PromptTooLong))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coordinate(generation: u64) -> TerminalCoordinate {
        TerminalCoordinate::new(42, "turn-7", "dispatch-9", generation).unwrap()
    }

    fn authority(generation: u64) -> TerminalAuthority {
        TerminalAuthority::new(
            coordinate(generation),
            TmuxSessionId(7001),
            TerminalReason::Provider(ProviderErrorStatus::Http429),
        )
        .unwrap()
    }

    fn attempt(generation: u64, attempt: u32) -> AuthClearAttempt {
        AuthClearAttempt::new(coordinate(generation), attempt).unwrap()
    }

    #[test]
    fn authority_constructor_requires_the_full_exact_tuple_and_kill_id() {
        for incomplete in [
            TerminalCoordinate::new(0, "turn-7", "dispatch-9", 1),
            TerminalCoordinate::new(42, "", "dispatch-9", 1),
            TerminalCoordinate::new(42, "turn-7", "", 1),
            TerminalCoordinate::new(42, "turn-7", "dispatch-9", 0),
        ] {
            assert!(incomplete.is_none());
        }
        assert!(
            TerminalAuthority::new(
                coordinate(1),
                TmuxSessionId(0),
                TerminalReason::Authentication,
            )
            .is_none()
        );
    }

    #[test]
    fn legacy_and_incomplete_no_row_evidence_are_observe_only() {
        assert_eq!(
            classify_no_row_action(NoRowEvidence::Legacy, None),
            NoRowAction::ObserveOnly(ClosedNotice::MissingExactTerminalTuple)
        );
        assert_eq!(
            classify_no_row_action(NoRowEvidence::Incomplete, None),
            NoRowAction::ObserveOnly(ClosedNotice::InvalidExactTerminalTuple)
        );
    }

    #[test]
    fn exact_no_row_authority_is_generation_isolated() {
        for generation in 1..64 {
            let current = authority(generation);
            assert_eq!(
                classify_no_row_action(NoRowEvidence::Exact(current.clone()), None),
                NoRowAction::Authorize(current.clone())
            );
            assert_eq!(
                classify_no_row_action(
                    NoRowEvidence::Exact(current),
                    Some(&coordinate(generation + 1)),
                ),
                NoRowAction::ObserveOnly(ClosedNotice::NewerDispatchPending)
            );
        }
    }

    #[test]
    fn exact_matching_pending_dispatch_does_not_create_cross_generation_authority() {
        let current = authority(11);
        assert_eq!(
            classify_no_row_action(NoRowEvidence::Exact(current.clone()), Some(&coordinate(11)),),
            NoRowAction::Authorize(current)
        );
    }

    #[test]
    fn dispatch_claim_admission_rejects_n_to_n_plus_one_and_replay() {
        let authority = authority(8);
        let exact = DispatchClaim::new(coordinate(8), "claim-a").unwrap();
        let newer = DispatchClaim::new(coordinate(9), "claim-b").unwrap();

        assert_eq!(
            admit_dispatch_claim(&authority, None, exact.clone()),
            DispatchAdmission::Admitted(exact.clone())
        );
        assert_eq!(
            admit_dispatch_claim(&authority, Some(&exact), exact.clone()),
            DispatchAdmission::ObserveOnly(ClosedNotice::ClaimReplay)
        );
        assert_eq!(
            admit_dispatch_claim(&authority, None, newer),
            DispatchAdmission::ObserveOnly(ClosedNotice::CoordinateMismatch)
        );
    }

    #[test]
    fn dispatch_claim_conflict_never_replaces_the_existing_claim() {
        let authority = authority(3);
        let first = DispatchClaim::new(coordinate(3), "claim-a").unwrap();
        let second = DispatchClaim::new(coordinate(3), "claim-b").unwrap();
        assert_eq!(
            admit_dispatch_claim(&authority, Some(&first), second),
            DispatchAdmission::ObserveOnly(ClosedNotice::ConflictingClaim)
        );
    }

    #[test]
    fn only_actual_exact_target_killed_unlocks_kill_settlement() {
        let authority = authority(1);
        let cases = [
            (
                KillOutcome::Killed {
                    session_id: TmuxSessionId(7001),
                },
                KillNotice::Killed,
                SettlementPermission::Allowed,
                None,
            ),
            (
                KillOutcome::Killed {
                    session_id: TmuxSessionId(7002),
                },
                KillNotice::Preserved,
                SettlementPermission::Blocked,
                Some(ClosedNotice::KillTargetMismatch),
            ),
            (
                KillOutcome::PreservedIdentityChanged {
                    expected: TmuxSessionId(7001),
                    observed: TmuxSessionId(7002),
                },
                KillNotice::Preserved,
                SettlementPermission::Blocked,
                Some(ClosedNotice::KillNotConfirmed),
            ),
            (
                KillOutcome::CaptureFailed,
                KillNotice::Preserved,
                SettlementPermission::Blocked,
                Some(ClosedNotice::KillNotConfirmed),
            ),
            (
                KillOutcome::TimedOut,
                KillNotice::Preserved,
                SettlementPermission::Blocked,
                Some(ClosedNotice::KillNotConfirmed),
            ),
            (
                KillOutcome::CommandFailed,
                KillNotice::Preserved,
                SettlementPermission::Blocked,
                Some(ClosedNotice::KillNotConfirmed),
            ),
        ];

        for (outcome, notice, settlement, closed_notice) in cases {
            assert_eq!(
                reduce_kill_result(&authority, outcome),
                KillReduction {
                    notice,
                    settlement,
                    closed_notice,
                }
            );
        }
    }

    #[test]
    fn auth_failures_keep_a_durable_retry_pending_and_block_settlement() {
        for outcome in [
            AuthClearOutcome::PersistFailed(attempt(5, 1)),
            AuthClearOutcome::Stale(attempt(5, 1)),
        ] {
            let reduction =
                reduce_auth_clear_obligation(&AuthClearObligation::Pending(attempt(5, 1)), outcome);
            assert_eq!(
                reduction.obligation,
                AuthClearObligation::Pending(attempt(5, 2))
            );
            assert_eq!(reduction.retry, AuthRetryObligation::Pending(attempt(5, 2)));
            assert_eq!(reduction.settlement, SettlementPermission::Blocked);
            assert_eq!(
                reduction.closed_notice,
                Some(ClosedNotice::AuthClearPending)
            );
        }
    }

    #[test]
    fn auth_retry_success_is_consumed_exactly_once() {
        let failed = reduce_auth_clear_obligation(
            &AuthClearObligation::Pending(attempt(5, 1)),
            AuthClearOutcome::PersistFailed(attempt(5, 1)),
        );
        let cleared = reduce_auth_clear_obligation(
            &failed.obligation,
            AuthClearOutcome::Cleared(attempt(5, 2)),
        );
        assert_eq!(
            cleared.obligation,
            AuthClearObligation::Satisfied(attempt(5, 2))
        );
        assert_eq!(cleared.retry, AuthRetryObligation::None);
        assert_eq!(cleared.settlement, SettlementPermission::Allowed);

        let replay = reduce_auth_clear_obligation(
            &cleared.obligation,
            AuthClearOutcome::Cleared(attempt(5, 2)),
        );
        assert_eq!(replay.obligation, cleared.obligation);
        assert_eq!(replay.retry, AuthRetryObligation::None);
        assert_eq!(replay.closed_notice, Some(ClosedNotice::AuthAlreadyCleared));
    }

    #[test]
    fn auth_outcomes_are_attempt_and_generation_ordered() {
        let pending = AuthClearObligation::Pending(attempt(7, 2));
        for stale in [attempt(7, 1), attempt(7, 3), attempt(8, 2)] {
            let reduction =
                reduce_auth_clear_obligation(&pending, AuthClearOutcome::Cleared(stale));
            assert_eq!(reduction.obligation, pending);
            assert_eq!(reduction.retry, AuthRetryObligation::Pending(attempt(7, 2)));
            assert_eq!(reduction.settlement, SettlementPermission::Blocked);
            assert_eq!(
                reduction.closed_notice,
                Some(ClosedNotice::AuthOutcomeStale)
            );
        }
    }

    #[test]
    fn auth_not_required_is_already_settleable_without_creating_a_retry() {
        let reduction = reduce_auth_clear_obligation(
            &AuthClearObligation::NotRequired,
            AuthClearOutcome::PersistFailed(attempt(1, 1)),
        );
        assert_eq!(reduction.obligation, AuthClearObligation::NotRequired);
        assert_eq!(reduction.retry, AuthRetryObligation::None);
        assert_eq!(reduction.settlement, SettlementPermission::Allowed);
    }

    #[test]
    fn closed_provider_status_grammar_accepts_only_designated_exact_forms() {
        let accepted = [
            ("[API Error: status=429]", ProviderErrorStatus::Http429),
            ("[API Error: status=529]", ProviderErrorStatus::Http529),
            (
                "[API Error: code=rate_limit_exceeded]",
                ProviderErrorStatus::Code(ProviderErrorCode::RateLimitExceeded),
            ),
            (
                "[API Error: code=overloaded]",
                ProviderErrorStatus::Code(ProviderErrorCode::Overloaded),
            ),
            (
                "[API Error: code=authentication_failed]",
                ProviderErrorStatus::Code(ProviderErrorCode::AuthenticationFailed),
            ),
            (
                "[API Error: code=prompt_too_long]",
                ProviderErrorStatus::Code(ProviderErrorCode::PromptTooLong),
            ),
        ];
        for (input, status) in accepted {
            assert_eq!(parse_closed_provider_status(input), Some(status), "{input}");
        }
    }

    #[test]
    fn closed_provider_status_grammar_rejects_near_misses_and_ambiguity() {
        let rejected = [
            "",
            "status=429",
            "[API Error: 429]",
            "[API Error: status =429]",
            "[API Error: status=429 ]",
            "[API Error: status=429]\n",
            " [API Error: status=429]",
            "[API Error: status=429] retry later",
            "prose [API Error: status=429]",
            "\"[API Error: status=429]\"",
            "[[API Error: status=429]]",
            "[API Error: {status=429}]",
            "[API Error: status=42]",
            "[API Error: status=4290]",
            "[API Error: status=529 status=429]",
            "[API Error: status=429,code=overloaded]",
            "[API Error: code=overload]",
            "[API Error: code=OVERLOADED]",
            "[API Error: code=overloaded extra]",
            "[API Error: code=\"overloaded\"]",
            "[API Error: code=overloaded][API Error: status=529]",
            "The rate limit is documented here.",
            "authentication_failed",
            "prompt_too_long",
        ];
        for input in rejected {
            assert_eq!(parse_closed_provider_status(input), None, "{input}");
        }
    }

    #[test]
    fn arbitrary_prefix_suffix_property_never_grants_provider_status() {
        let valid = "[API Error: status=429]";
        for token in ["x", " ", "\n", "'", "[", "]", "status=529"] {
            assert_eq!(
                parse_closed_provider_status(&format!("{token}{valid}")),
                None
            );
            assert_eq!(
                parse_closed_provider_status(&format!("{valid}{token}")),
                None
            );
        }
    }
}
