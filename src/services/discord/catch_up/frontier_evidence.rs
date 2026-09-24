//! #6035 — the one answer to "may the frontier pass this id?" for both phases.
//! Constructors list every variant, so a new arm or commit must be classified.

use super::super::recovery_known_ids::RecoveryKnownIdArm;
use super::classification::CatchUpClassification;
use super::phase2::Phase2EnqueueCommit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FrontierEvidence {
    /// I20 evidence of dispatch or answer (or no turn is owed): the frontier may pass.
    Dispatched,
    /// No evidence yet: the frontier must stop before this id.
    Open,
}

impl FrontierEvidence {
    /// A skipped id: only a membership duplicate needs an arm's evidence.
    pub(super) fn of_known(
        outcome: CatchUpClassification,
        arm: Option<RecoveryKnownIdArm>,
    ) -> Self {
        match outcome {
            CatchUpClassification::Duplicate => arm.map_or(Self::Open, of_arm),
            CatchUpClassification::SystemKind
            | CatchUpClassification::SelfAuthored
            | CatchUpClassification::TooOld
            | CatchUpClassification::Settled
            | CatchUpClassification::Empty
            | CatchUpClassification::NotAllowed => Self::Dispatched,
            CatchUpClassification::Recover => Self::Open,
        }
    }

    pub(super) fn of_commit(commit: Phase2EnqueueCommit) -> Self {
        match commit {
            Phase2EnqueueCommit::Accepted | Phase2EnqueueCommit::DuplicateActiveTurn => {
                Self::Dispatched
            }
            Phase2EnqueueCommit::DuplicateQueued
            | Phase2EnqueueCommit::NotYetEvidenced
            | Phase2EnqueueCommit::LastItemDedup
            | Phase2EnqueueCommit::Deferred => Self::Open,
        }
    }
}

/// I20: only a turn that took THIS id is evidence; membership and absorption are not.
fn of_arm(arm: RecoveryKnownIdArm) -> FrontierEvidence {
    match arm {
        RecoveryKnownIdArm::ActiveTurn => FrontierEvidence::Dispatched,
        RecoveryKnownIdArm::Queued
        | RecoveryKnownIdArm::PendingDispatch
        | RecoveryKnownIdArm::AbsorbedActiveTurn => FrontierEvidence::Open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #5996/#6035: only the active-turn arm names a message a turn actually
    /// took, so only it may move a checkpoint past one.
    #[test]
    fn only_the_active_turn_arm_is_dispatch_evidence() {
        let duplicate = |arm| FrontierEvidence::of_known(CatchUpClassification::Duplicate, arm);
        assert_eq!(
            duplicate(Some(RecoveryKnownIdArm::ActiveTurn)),
            FrontierEvidence::Dispatched
        );
        for arm in [
            RecoveryKnownIdArm::Queued,
            RecoveryKnownIdArm::PendingDispatch,
            RecoveryKnownIdArm::AbsorbedActiveTurn,
        ] {
            assert_eq!(duplicate(Some(arm)), FrontierEvidence::Open, "{arm:?}");
        }
        assert_eq!(duplicate(None), FrontierEvidence::Open);
    }

    #[test]
    fn an_absorbed_active_turn_refusal_is_not_evidence() {
        let open = FrontierEvidence::of_commit(Phase2EnqueueCommit::NotYetEvidenced);
        assert_eq!(open, FrontierEvidence::Open);
        let taken = FrontierEvidence::of_commit(Phase2EnqueueCommit::DuplicateActiveTurn);
        assert_eq!(taken, FrontierEvidence::Dispatched);
    }
}
