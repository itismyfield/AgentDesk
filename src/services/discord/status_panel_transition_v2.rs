//! Dormant status-panel transition model for #4891.
//!
//! This module is intentionally pure and has no production caller. It cannot
//! perform Discord I/O, persist state, mutate legacy stores, or replace legacy
//! authority. A later slice must provide separately reviewed adapter wiring.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Candidate {
    pub channel_id: u64,
    pub message_id: u64,
    pub generation: u64,
    pub turn_id: u64,
    pub expected_prior_message_id: Option<u64>,
}

impl Candidate {
    fn is_valid(self) -> bool {
        self.channel_id != 0
            && self.message_id != 0
            && self.generation != 0
            && self.turn_id != 0
            && self.expected_prior_message_id != Some(0)
            && self.expected_prior_message_id != Some(self.message_id)
    }
}

/// Protection evidence presented at the bind boundary.
///
/// The legacy variants are observations only. They deliberately cannot satisfy
/// either initial or recovery bind authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProtectionEvidence {
    Missing,
    PendingBind,
    CandidateAcknowledged,
    JournalOwned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BindGuard {
    pub candidate: Candidate,
    pub protection: ProtectionEvidence,
    pub candidate_identity_matches: bool,
    pub turn_identity_matches: bool,
    pub generation_matches: bool,
    pub expected_binding_matches: bool,
}

impl BindGuard {
    fn is_complete_for(self, candidate: Candidate) -> bool {
        candidate.is_valid()
            && self.candidate == candidate
            && self.protection == ProtectionEvidence::JournalOwned
            && self.candidate_identity_matches
            && self.turn_identity_matches
            && self.generation_matches
            && self.expected_binding_matches
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BoundPanel {
    pub candidate: Candidate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailurePhase {
    Prepare,
    AuthorizeBind,
    CommitBind,
    AuthorizeRetire,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailureReason {
    GuardRejected,
    CommitFailed,
    AdapterFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QuarantineReason {
    MalformedRecord,
    UnknownState,
    InvariantViolation,
}

/// The future journal's tagged state.
///
/// `PendingBind` and `CandidateAcknowledged` are intentionally absent: legacy
/// observations are not states in this authority model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JournalState {
    Prepared {
        candidate: Candidate,
    },
    BindAuthorized {
        candidate: Candidate,
        guard: BindGuard,
    },
    Bound {
        panel: BoundPanel,
    },
    RetireAuthorized {
        panel: BoundPanel,
        delete_message_id: u64,
    },
    Retired {
        panel: BoundPanel,
        retired_message_id: u64,
    },
    Failed {
        phase: FailurePhase,
        reason: FailureReason,
    },
    Quarantined {
        reason: QuarantineReason,
    },
}

impl JournalState {
    pub(super) fn prepared(candidate: Candidate) -> Self {
        Self::Prepared { candidate }
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Retired { .. } | Self::Failed { .. } | Self::Quarantined { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransitionEvent {
    AuthorizeBind {
        guard: BindGuard,
    },
    CommitBind,
    AuthorizeRetire {
        delete_message_id: u64,
    },
    CommitRetire,
    Fail {
        phase: FailurePhase,
        reason: FailureReason,
    },
    Quarantine {
        reason: QuarantineReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransitionError {
    IllegalTransition,
    IncompleteBindGuard,
    InvalidRetirementTarget,
    TerminalState,
}

/// Applies one legal monotonic transition without performing any side effect.
pub(super) fn transition(
    state: JournalState,
    event: TransitionEvent,
) -> Result<JournalState, TransitionError> {
    if state.is_terminal() {
        return Err(TransitionError::TerminalState);
    }

    match (state, event) {
        (JournalState::Prepared { candidate }, TransitionEvent::AuthorizeBind { guard }) => {
            if !guard.is_complete_for(candidate) {
                return Err(TransitionError::IncompleteBindGuard);
            }
            Ok(JournalState::BindAuthorized { candidate, guard })
        }
        (JournalState::BindAuthorized { candidate, guard }, TransitionEvent::CommitBind)
            if guard.is_complete_for(candidate) =>
        {
            Ok(JournalState::Bound {
                panel: BoundPanel { candidate },
            })
        }
        (JournalState::Bound { panel }, TransitionEvent::AuthorizeRetire { delete_message_id }) => {
            if delete_message_id == 0
                || delete_message_id == panel.candidate.message_id
                || panel.candidate.expected_prior_message_id != Some(delete_message_id)
            {
                return Err(TransitionError::InvalidRetirementTarget);
            }
            Ok(JournalState::RetireAuthorized {
                panel,
                delete_message_id,
            })
        }
        (
            JournalState::RetireAuthorized {
                panel,
                delete_message_id,
            },
            TransitionEvent::CommitRetire,
        ) => Ok(JournalState::Retired {
            panel,
            retired_message_id: delete_message_id,
        }),
        (_, TransitionEvent::Fail { phase, reason }) => Ok(JournalState::Failed { phase, reason }),
        (_, TransitionEvent::Quarantine { reason }) => Ok(JournalState::Quarantined { reason }),
        _ => Err(TransitionError::IllegalTransition),
    }
}

/// Recovery may bind only the exact journal-owned candidate whose complete
/// guard was durably represented by `BindAuthorized`.
pub(super) fn recovery_bind_is_authorized(state: JournalState) -> bool {
    matches!(
        state,
        JournalState::BindAuthorized { candidate, guard }
            if guard.protection == ProtectionEvidence::JournalOwned
                && guard.is_complete_for(candidate)
    )
}

/// Physical deletion may begin only from the explicit retirement authority
/// state and only for its exact target.
pub(super) fn deletion_is_authorized(state: JournalState, message_id: u64) -> bool {
    matches!(
        state,
        JournalState::RetireAuthorized {
            delete_message_id,
            ..
        } if message_id != 0 && message_id == delete_message_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANDIDATE: Candidate = Candidate {
        channel_id: 11,
        message_id: 22,
        generation: 33,
        turn_id: 44,
        expected_prior_message_id: Some(21),
    };

    fn complete_guard() -> BindGuard {
        BindGuard {
            candidate: CANDIDATE,
            protection: ProtectionEvidence::JournalOwned,
            candidate_identity_matches: true,
            turn_identity_matches: true,
            generation_matches: true,
            expected_binding_matches: true,
        }
    }

    fn prepared() -> JournalState {
        JournalState::prepared(CANDIDATE)
    }

    fn bind_authorized() -> JournalState {
        transition(
            prepared(),
            TransitionEvent::AuthorizeBind {
                guard: complete_guard(),
            },
        )
        .unwrap()
    }

    fn bound() -> JournalState {
        transition(bind_authorized(), TransitionEvent::CommitBind).unwrap()
    }

    fn retire_authorized() -> JournalState {
        transition(
            bound(),
            TransitionEvent::AuthorizeRetire {
                delete_message_id: 21,
            },
        )
        .unwrap()
    }

    fn failed() -> JournalState {
        JournalState::Failed {
            phase: FailurePhase::CommitBind,
            reason: FailureReason::CommitFailed,
        }
    }

    fn quarantined() -> JournalState {
        JournalState::Quarantined {
            reason: QuarantineReason::InvariantViolation,
        }
    }

    fn retired() -> JournalState {
        transition(retire_authorized(), TransitionEvent::CommitRetire).unwrap()
    }

    #[test]
    fn happy_path_is_strictly_monotonic() {
        let authorized = bind_authorized();
        assert!(recovery_bind_is_authorized(authorized));

        let bound = transition(authorized, TransitionEvent::CommitBind).unwrap();
        assert!(!recovery_bind_is_authorized(bound));

        let retire_authorized = transition(
            bound,
            TransitionEvent::AuthorizeRetire {
                delete_message_id: 21,
            },
        )
        .unwrap();
        assert!(deletion_is_authorized(retire_authorized, 21));

        let retired = transition(retire_authorized, TransitionEvent::CommitRetire).unwrap();
        assert!(!deletion_is_authorized(retired, 21));
    }

    #[test]
    fn exhaustive_normal_transition_table_accepts_only_adjacent_edges() {
        #[derive(Clone, Copy)]
        enum StateCase {
            Prepared,
            BindAuthorized,
            Bound,
            RetireAuthorized,
            Retired,
            Failed,
            Quarantined,
        }

        #[derive(Clone, Copy)]
        enum EventCase {
            AuthorizeBind,
            CommitBind,
            AuthorizeRetire,
            CommitRetire,
        }

        let states = [
            StateCase::Prepared,
            StateCase::BindAuthorized,
            StateCase::Bound,
            StateCase::RetireAuthorized,
            StateCase::Retired,
            StateCase::Failed,
            StateCase::Quarantined,
        ];
        let events = [
            EventCase::AuthorizeBind,
            EventCase::CommitBind,
            EventCase::AuthorizeRetire,
            EventCase::CommitRetire,
        ];

        for (state_index, state_case) in states.into_iter().enumerate() {
            for (event_index, event_case) in events.into_iter().enumerate() {
                let state = match state_case {
                    StateCase::Prepared => prepared(),
                    StateCase::BindAuthorized => bind_authorized(),
                    StateCase::Bound => bound(),
                    StateCase::RetireAuthorized => retire_authorized(),
                    StateCase::Retired => retired(),
                    StateCase::Failed => failed(),
                    StateCase::Quarantined => quarantined(),
                };
                let event = match event_case {
                    EventCase::AuthorizeBind => TransitionEvent::AuthorizeBind {
                        guard: complete_guard(),
                    },
                    EventCase::CommitBind => TransitionEvent::CommitBind,
                    EventCase::AuthorizeRetire => TransitionEvent::AuthorizeRetire {
                        delete_message_id: 21,
                    },
                    EventCase::CommitRetire => TransitionEvent::CommitRetire,
                };
                let accepted = transition(state, event).is_ok();
                assert_eq!(
                    accepted,
                    state_index == event_index,
                    "state {state_index}, event {event_index}"
                );
            }
        }
    }

    #[test]
    fn failure_and_quarantine_are_terminal_from_every_live_state() {
        let live_states = [prepared(), bind_authorized(), bound(), retire_authorized()];

        for state in live_states {
            let failed = transition(
                state,
                TransitionEvent::Fail {
                    phase: FailurePhase::Prepare,
                    reason: FailureReason::AdapterFailed,
                },
            )
            .unwrap();
            assert!(matches!(failed, JournalState::Failed { .. }));

            let quarantined = transition(
                state,
                TransitionEvent::Quarantine {
                    reason: QuarantineReason::MalformedRecord,
                },
            )
            .unwrap();
            assert!(matches!(quarantined, JournalState::Quarantined { .. }));
        }

        for terminal in [retired(), failed(), quarantined()] {
            assert_eq!(
                transition(
                    terminal,
                    TransitionEvent::Quarantine {
                        reason: QuarantineReason::UnknownState,
                    },
                ),
                Err(TransitionError::TerminalState)
            );
        }
    }

    #[test]
    fn every_bind_guard_clause_is_required() {
        let mutations: [(&str, fn(&mut BindGuard)); 6] = [
            ("journal ownership", |guard| {
                guard.protection = ProtectionEvidence::Missing
            }),
            ("candidate identity", |guard| {
                guard.candidate_identity_matches = false
            }),
            ("turn identity", |guard| guard.turn_identity_matches = false),
            ("generation", |guard| guard.generation_matches = false),
            ("expected binding", |guard| {
                guard.expected_binding_matches = false
            }),
            ("exact candidate", |guard| guard.candidate.message_id += 1),
        ];

        for (name, mutate) in mutations {
            let mut guard = complete_guard();
            mutate(&mut guard);
            assert_eq!(
                transition(prepared(), TransitionEvent::AuthorizeBind { guard }),
                Err(TransitionError::IncompleteBindGuard),
                "removed guard: {name}"
            );
        }
    }

    #[test]
    fn legacy_pending_bind_and_candidate_acknowledged_never_authorize_bind() {
        for protection in [
            ProtectionEvidence::PendingBind,
            ProtectionEvidence::CandidateAcknowledged,
        ] {
            let mut guard = complete_guard();
            guard.protection = protection;
            assert_eq!(
                transition(prepared(), TransitionEvent::AuthorizeBind { guard }),
                Err(TransitionError::IncompleteBindGuard)
            );

            let forged = JournalState::BindAuthorized {
                candidate: CANDIDATE,
                guard,
            };
            assert!(!recovery_bind_is_authorized(forged));
            assert_eq!(
                transition(forged, TransitionEvent::CommitBind),
                Err(TransitionError::IllegalTransition)
            );
        }
    }

    #[test]
    fn recovery_bind_requires_bind_authorized_state_and_complete_journal_guard() {
        let live_non_authority = [prepared(), bound(), retire_authorized()];
        for state in live_non_authority {
            assert!(!recovery_bind_is_authorized(state));
        }

        let mutations: [fn(&mut BindGuard); 6] = [
            |guard| guard.protection = ProtectionEvidence::Missing,
            |guard| guard.candidate_identity_matches = false,
            |guard| guard.turn_identity_matches = false,
            |guard| guard.generation_matches = false,
            |guard| guard.expected_binding_matches = false,
            |guard| guard.candidate.turn_id += 1,
        ];
        for mutate in mutations {
            let mut guard = complete_guard();
            mutate(&mut guard);
            assert!(!recovery_bind_is_authorized(JournalState::BindAuthorized {
                candidate: CANDIDATE,
                guard,
            }));
        }
        assert!(recovery_bind_is_authorized(bind_authorized()));
    }

    #[test]
    fn deletion_requires_retire_authorized_and_exact_target() {
        for state in [prepared(), bind_authorized(), bound(), retired(), failed()] {
            assert!(!deletion_is_authorized(state, 21));
        }
        let state = retire_authorized();
        assert!(!deletion_is_authorized(state, 0));
        assert!(!deletion_is_authorized(state, 20));
        assert!(!deletion_is_authorized(state, CANDIDATE.message_id));
        assert!(deletion_is_authorized(state, 21));
    }

    #[test]
    fn retirement_rejects_current_or_unexpected_message() {
        for delete_message_id in [0, 20, CANDIDATE.message_id] {
            assert_eq!(
                transition(
                    bound(),
                    TransitionEvent::AuthorizeRetire { delete_message_id },
                ),
                Err(TransitionError::InvalidRetirementTarget)
            );
        }
    }

    #[test]
    fn invalid_candidate_cannot_receive_bind_authority() {
        let invalid_candidates = [
            Candidate {
                channel_id: 0,
                ..CANDIDATE
            },
            Candidate {
                message_id: 0,
                ..CANDIDATE
            },
            Candidate {
                generation: 0,
                ..CANDIDATE
            },
            Candidate {
                turn_id: 0,
                ..CANDIDATE
            },
            Candidate {
                expected_prior_message_id: Some(0),
                ..CANDIDATE
            },
            Candidate {
                expected_prior_message_id: Some(CANDIDATE.message_id),
                ..CANDIDATE
            },
        ];

        for candidate in invalid_candidates {
            let mut guard = complete_guard();
            guard.candidate = candidate;
            assert_eq!(
                transition(
                    JournalState::prepared(candidate),
                    TransitionEvent::AuthorizeBind { guard },
                ),
                Err(TransitionError::IncompleteBindGuard)
            );
        }
    }

    #[test]
    fn forged_incomplete_bind_authority_cannot_commit() {
        let mut guard = complete_guard();
        guard.generation_matches = false;
        let forged = JournalState::BindAuthorized {
            candidate: CANDIDATE,
            guard,
        };
        assert_eq!(
            transition(forged, TransitionEvent::CommitBind),
            Err(TransitionError::IllegalTransition)
        );
    }

    #[test]
    fn proof_model_has_no_legacy_or_io_authority_surface() {
        let source = include_str!("status_panel_transition_v2.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "serenity",
            "reqwest",
            "std::fs",
            "tokio::fs",
            "status_panel_orphan_store",
            "status_panel_singleton_store",
            "inflight::",
        ] {
            assert!(
                !production.contains(forbidden),
                "forbidden production authority surface: {forbidden}"
            );
        }
    }

    #[test]
    fn failure_taxonomy_covers_each_transition_phase() {
        let phases = [
            FailurePhase::Prepare,
            FailurePhase::AuthorizeBind,
            FailurePhase::CommitBind,
            FailurePhase::AuthorizeRetire,
            FailurePhase::Delete,
        ];
        let reasons = [
            FailureReason::GuardRejected,
            FailureReason::CommitFailed,
            FailureReason::AdapterFailed,
        ];
        for phase in phases {
            for reason in reasons {
                assert_eq!(
                    transition(prepared(), TransitionEvent::Fail { phase, reason }),
                    Ok(JournalState::Failed { phase, reason })
                );
            }
        }
    }

    #[test]
    fn quarantine_taxonomy_remains_typed() {
        for reason in [
            QuarantineReason::MalformedRecord,
            QuarantineReason::UnknownState,
            QuarantineReason::InvariantViolation,
        ] {
            assert_eq!(
                transition(prepared(), TransitionEvent::Quarantine { reason }),
                Ok(JournalState::Quarantined { reason })
            );
        }
    }
}
