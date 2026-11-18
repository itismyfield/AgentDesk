use serde::{Deserialize, Serialize};

use crate::services::campaign_execution::model::ActionKind;
use crate::services::campaign_execution::verification_run::{
    AuthoritySource, Digest, VerificationStartAuthority, phase_action_id,
};

use super::EffectIdentity;

// Who may start an effect now. It is never part of the effect's identity or payload, so a handover
// changes the holder without changing the effect or its action ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "holder", rename_all = "snake_case")]
pub enum EffectAuthority {
    CurrentAttempt {
        attempt_id: String,
        binding_digest: Digest,
        node_epoch: u64,
        run_control_version: u64,
    },
    Verification(VerificationStartAuthority),
    LandingMerge {
        action_id: String,
        binding_digest: Digest,
        node_epoch: u64,
        run_control_version: u64,
    },
    Landing {
        action_id: String,
        binding_digest: Digest,
        node_epoch: u64,
        run_control_version: u64,
    },
}

impl EffectAuthority {
    pub(super) fn fence(&self) -> (u64, u64) {
        match self {
            Self::CurrentAttempt {
                run_control_version,
                node_epoch,
                ..
            }
            | Self::LandingMerge {
                run_control_version,
                node_epoch,
                ..
            }
            | Self::Landing {
                run_control_version,
                node_epoch,
                ..
            } => (*run_control_version, *node_epoch),
            Self::Verification(a) => (a.run_control_version, a.consumer_node_epoch),
        }
    }

    // An authority only ever speaks for the effect it was resolved for.
    pub(super) fn names(&self, effect: &EffectIdentity) -> bool {
        match self {
            Self::CurrentAttempt {
                attempt_id,
                binding_digest,
                ..
            } => {
                !matches!(
                    effect.kind,
                    ActionKind::VerificationPhase | ActionKind::CreatePr
                ) && effect.phase.is_none()
                    && (attempt_id, binding_digest)
                        == (&effect.origin_attempt_id, &effect.origin_binding_digest)
            }
            Self::LandingMerge {
                action_id,
                binding_digest,
                ..
            } => {
                effect.kind == ActionKind::Merge
                    && effect.phase.is_none()
                    && (action_id, binding_digest)
                        == (&effect.action_id, &effect.origin_binding_digest)
            }
            Self::Landing {
                action_id,
                binding_digest,
                ..
            } => {
                effect.kind == ActionKind::CreatePr
                    && effect.phase.is_none()
                    && (action_id, binding_digest)
                        == (&effect.action_id, &effect.origin_binding_digest)
            }
            Self::Verification(a) => effect.phase.as_ref().is_some_and(|p| {
                effect.kind == ActionKind::VerificationPhase
                    && a.original_binding_digest == effect.origin_binding_digest
                    && a.execution_id == p.execution_id
                    && effect.action_id
                        == phase_action_id(&effect.run_id, &p.execution_id, &p.phase_id)
            }),
        }
    }

    pub(super) fn finish_plan(&self) -> bool {
        matches!(self, Self::Verification(a) if matches!(a.source, AuthoritySource::FinishPlan { .. }))
    }
}
