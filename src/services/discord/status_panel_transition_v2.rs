//! Dormant status-panel transition model and filesystem journal for #4891.
//!
//! This module has no production caller. It cannot perform Discord I/O, mutate
//! legacy stores, replace legacy authority, or activate a cutover. The journal
//! adapter models durable authorization that a later slice may wire separately.

mod journal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PanelIdentity {
    provider: journal::CanonicalProvider,
    canonical_token_hash: journal::CanonicalTokenHash,
    channel_id: u64,
}

impl PanelIdentity {
    pub(super) fn provider(&self) -> &str {
        self.provider.as_str()
    }

    pub(super) fn canonical_token_hash(&self) -> &str {
        self.canonical_token_hash.as_str()
    }

    pub(super) fn channel_id(&self) -> u64 {
        self.channel_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PanelPlan {
    pub identity: PanelIdentity,
    pub turn_id: u64,
    pub expected_prior_message_id: Option<u64>,
}

impl PanelPlan {
    fn is_valid(&self) -> bool {
        self.identity.channel_id != 0
            && self.turn_id != 0
            && self.expected_prior_message_id != Some(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Candidate {
    pub plan: PanelPlan,
    pub message_id: u64,
    pub generation: u64,
}

impl Candidate {
    fn is_valid(&self) -> bool {
        self.plan.is_valid()
            && self.message_id != 0
            && self.generation != 0
            && self.plan.expected_prior_message_id != Some(self.message_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BoundPanel {
    pub candidate: Candidate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailurePhase {
    Prepare,
    RecordSent,
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

/// Tagged state consumed by the dormant journal adapter.
///
/// The journal-owned handle has private fields and no constructor outside the
/// adapter. Legacy observations are intentionally absent from this bind model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum JournalState {
    Prepared {
        plan: PanelPlan,
        generation: u64,
    },
    BindAuthorized {
        candidate: Candidate,
        authorization: journal::JournalOwnedBind,
    },
    Bound {
        panel: BoundPanel,
    },
    RetireAuthorized {
        panel: BoundPanel,
        authorization: journal::JournalOwnedRetire,
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
    pub(super) fn prepared(plan: PanelPlan, generation: u64) -> Self {
        Self::Prepared { plan, generation }
    }

    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Retired { .. } | Self::Failed { .. } | Self::Quarantined { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TransitionEvent {
    AuthorizeBind {
        authorization: journal::JournalOwnedBind,
    },
    CommitBind,
    AuthorizeRetire {
        authorization: journal::JournalOwnedRetire,
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
    IncompleteBindAuthorization,
    InvalidRetirementAuthorization,
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
        (
            JournalState::Prepared { plan, generation },
            TransitionEvent::AuthorizeBind { authorization },
        ) => {
            let candidate = authorization.candidate().clone();
            if !candidate.is_valid()
                || candidate.plan != plan
                || candidate.generation != generation
                || !authorization.matches_candidate(&candidate)
            {
                return Err(TransitionError::IncompleteBindAuthorization);
            }
            Ok(JournalState::BindAuthorized {
                candidate,
                authorization,
            })
        }
        (
            JournalState::BindAuthorized {
                candidate,
                authorization,
            },
            TransitionEvent::CommitBind,
        ) if candidate.is_valid() && authorization.matches_candidate(&candidate) => {
            Ok(JournalState::Bound {
                panel: BoundPanel { candidate },
            })
        }
        (JournalState::Bound { panel }, TransitionEvent::AuthorizeRetire { authorization }) => {
            if !authorization.matches_panel(&panel) {
                return Err(TransitionError::InvalidRetirementAuthorization);
            }
            Ok(JournalState::RetireAuthorized {
                panel,
                authorization,
            })
        }
        (
            JournalState::RetireAuthorized {
                panel,
                authorization,
            },
            TransitionEvent::CommitRetire,
        ) if authorization.matches_panel(&panel) => Ok(JournalState::Retired {
            retired_message_id: authorization.delete_message_id(),
            panel,
        }),
        (_, TransitionEvent::Fail { phase, reason }) => Ok(JournalState::Failed { phase, reason }),
        (_, TransitionEvent::Quarantine { reason }) => Ok(JournalState::Quarantined { reason }),
        _ => Err(TransitionError::IllegalTransition),
    }
}

/// Recovery may bind only an opaque authorization rehydrated from the exact
/// durable `BindAuthorized` operation record.
pub(super) fn recovery_bind_authorization(
    state: &JournalState,
) -> Option<&journal::JournalOwnedBind> {
    match state {
        JournalState::BindAuthorized {
            candidate,
            authorization,
        } if authorization.matches_candidate(candidate) => Some(authorization),
        _ => None,
    }
}

/// Physical deletion may begin only with the exact durable retirement handle.
pub(super) fn deletion_authorization(state: &JournalState) -> Option<&journal::JournalOwnedRetire> {
    match state {
        JournalState::RetireAuthorized {
            panel,
            authorization,
        } if authorization.matches_panel(panel) => Some(authorization),
        _ => None,
    }
}

/// Legacy inputs remain observations only and have no conversion into bind
/// authorization. A later migration may separately evaluate them for retirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LegacyRetireObservation {
    pub identity: PanelIdentity,
    pub message_id: u64,
}

impl LegacyRetireObservation {
    pub(super) fn new(identity: PanelIdentity, message_id: u64) -> Option<Self> {
        (message_id != 0).then_some(Self {
            identity,
            message_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> PanelPlan {
        PanelPlan {
            identity: journal::identity_for_test("claude", "discord_0123456789abcdef", 11),
            turn_id: 44,
            expected_prior_message_id: Some(21),
        }
    }

    fn candidate() -> Candidate {
        Candidate {
            plan: plan(),
            message_id: 22,
            generation: 33,
        }
    }

    fn bind_authorization() -> journal::JournalOwnedBind {
        journal::bind_authorization_for_test(candidate())
    }

    fn prepared() -> JournalState {
        JournalState::prepared(plan(), 33)
    }

    fn bind_authorized() -> JournalState {
        transition(
            prepared(),
            TransitionEvent::AuthorizeBind {
                authorization: bind_authorization(),
            },
        )
        .unwrap()
    }

    fn bound() -> JournalState {
        transition(bind_authorized(), TransitionEvent::CommitBind).unwrap()
    }

    fn retire_authorized() -> JournalState {
        let panel = match bound() {
            JournalState::Bound { panel } => panel,
            _ => unreachable!(),
        };
        transition(
            JournalState::Bound {
                panel: panel.clone(),
            },
            TransitionEvent::AuthorizeRetire {
                authorization: journal::retire_authorization_for_test(panel, 21),
            },
        )
        .unwrap()
    }

    #[test]
    fn happy_path_is_strictly_monotonic() {
        let authorized = bind_authorized();
        assert!(recovery_bind_authorization(&authorized).is_some());
        let bound = transition(authorized, TransitionEvent::CommitBind).unwrap();
        assert!(recovery_bind_authorization(&bound).is_none());

        let panel = match &bound {
            JournalState::Bound { panel } => panel.clone(),
            _ => unreachable!(),
        };
        let retire = transition(
            bound,
            TransitionEvent::AuthorizeRetire {
                authorization: journal::retire_authorization_for_test(panel, 21),
            },
        )
        .unwrap();
        assert_eq!(
            deletion_authorization(&retire).map(|proof| proof.delete_message_id()),
            Some(21)
        );
        let retired = transition(retire, TransitionEvent::CommitRetire).unwrap();
        assert!(deletion_authorization(&retired).is_none());
    }

    #[test]
    fn mismatched_adapter_authorizations_are_rejected() {
        let mut wrong_candidate = candidate();
        wrong_candidate.message_id += 1;
        assert_eq!(
            transition(
                prepared(),
                TransitionEvent::AuthorizeBind {
                    authorization: journal::mutate_bind_authorization_for_test(
                        bind_authorization(),
                        wrong_candidate,
                    ),
                },
            ),
            Err(TransitionError::IncompleteBindAuthorization)
        );

        let panel = match bound() {
            JournalState::Bound { panel } => panel,
            _ => unreachable!(),
        };
        assert_eq!(
            transition(
                JournalState::Bound {
                    panel: panel.clone(),
                },
                TransitionEvent::AuthorizeRetire {
                    authorization: journal::retire_authorization_for_test(panel, 20),
                },
            ),
            Err(TransitionError::InvalidRetirementAuthorization)
        );
    }

    #[test]
    fn failure_and_quarantine_are_terminal() {
        for state in [prepared(), bind_authorized(), bound(), retire_authorized()] {
            let failed = transition(
                state.clone(),
                TransitionEvent::Fail {
                    phase: FailurePhase::Prepare,
                    reason: FailureReason::AdapterFailed,
                },
            )
            .unwrap();
            assert_eq!(
                transition(failed, TransitionEvent::CommitBind),
                Err(TransitionError::TerminalState)
            );
            let quarantined = transition(
                state,
                TransitionEvent::Quarantine {
                    reason: QuarantineReason::MalformedRecord,
                },
            )
            .unwrap();
            assert_eq!(
                transition(quarantined, TransitionEvent::CommitBind),
                Err(TransitionError::TerminalState)
            );
        }
    }

    #[test]
    fn legacy_observation_has_no_bind_conversion_surface() {
        let observation = LegacyRetireObservation::new(plan().identity, 21).unwrap();
        assert_eq!(observation.message_id, 21);
        let source = include_str!("status_panel_transition_v2.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("impl From<LegacyRetireObservation"));
        assert!(!production.contains("JournalOwnedBind::new"));
    }

    #[test]
    fn model_has_no_discord_or_legacy_authority_surface() {
        let source = include_str!("status_panel_transition_v2.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "serenity",
            "reqwest",
            "status_panel_orphan_store",
            "status_panel_singleton_store",
            "inflight::",
        ] {
            assert!(
                !production.contains(forbidden),
                "forbidden surface: {forbidden}"
            );
        }
    }
}
