//! The admitted first output's one normalization, kept with its admission under the adoption
//! lock so every body claim of that turn reads the same terminal answer.

use super::*;
use crate::services::tui_o::channel_policy::output_disposition::{
    BUDGET, Identity, Output, Phase, Reason,
};
use tokio::time::Instant;

#[derive(Clone, Debug)]
pub(super) struct Normalizing {
    admitted_at: Instant,
    /// The disposition and the proof it was made from, once a bound rollout proved the admission.
    output: Option<(Output, FirstProof)>,
    /// Why the admission was refused when no rollout proved it within the budget.
    refused: Option<Reason>,
}

impl Normalizing {
    pub(super) fn admitted() -> Self {
        Self {
            admitted_at: Instant::now(),
            output: None,
            refused: None,
        }
    }
}

/// What a body claim of the admitted first turn waits on.
pub(crate) enum FirstHold {
    /// A bound rollout proved the admission: its disposition normalizes to a terminal answer.
    Output(Box<(Output, FirstProof)>),
    /// No rollout proved it yet; the one budget ends at this instant.
    Until(Instant),
}

impl Candidate {
    /// Starts the admitted first output's normalization from the proof `source` gives; the first
    /// proof stays, and its budget began at the admission.
    pub(crate) fn normalize_first(&self, source: &SourceId) {
        let guard = self.lock();
        if let Ok(Some(proof)) = self.first_proof(source) {
            self.first_turn.send_if_modified(|first| {
                let Some(first) = first
                    .as_mut()
                    .filter(|first| first.normalizing.output.is_none())
                else {
                    return false;
                };
                let id = Identity::from_proof(&proof, proof.mark.order);
                let output = Output::new(id, first.normalizing.admitted_at);
                first.normalizing.output = Some((output, proof));
                true
            });
        }
        drop(guard);
    }

    /// What a body claim of the turn `nonce` names waits on: nothing once its output is adopted
    /// or rejected, or when it is not the admitted first turn.
    pub(crate) fn first_hold(&self, nonce: [u8; 32]) -> Option<FirstHold> {
        let first = self.first_turn.borrow();
        let first = first.as_ref().filter(|first| {
            nonce_id(&first.nonce) == nonce && first.normalizing.refused.is_none()
        })?;
        match &first.normalizing.output {
            Some((output, _)) if terminal(output.phase()) => None,
            Some((output, proof)) => {
                Some(FirstHold::Output(Box::new((output.clone(), proof.clone()))))
            }
            None if first.pending => Some(FirstHold::Until(first.normalizing.admitted_at + BUDGET)),
            None => None,
        }
    }

    /// Refuses the waiting admission `nonce` names once its budget ended with nothing able to
    /// adopt it; an adoption decided meanwhile is kept.
    pub(crate) fn expire_first(&self, nonce: [u8; 32]) {
        let mut adoption = self.lock();
        if *adoption != Adoption::Deferred {
            return;
        }
        let expired = self.first_turn.send_if_modified(|first| {
            let Some(first) = first.as_mut().filter(|first| {
                nonce_id(&first.nonce) == nonce
                    && first.normalizing.refused.is_none()
                    && Instant::now() >= first.normalizing.admitted_at + BUDGET
            }) else {
                return false;
            };
            (first.valid, first.pending) = (false, false);
            first.normalizing.refused = Some(Reason::Deadline);
            true
        });
        if expired {
            *adoption = Adoption::Refused;
        }
    }

    /// The reason the admitted first output `nonce` names was rejected with, for its one notice.
    pub(crate) fn first_rejection(&self, nonce: [u8; 32]) -> Option<Reason> {
        let first = self.first_turn.borrow();
        let first = first
            .as_ref()
            .filter(|first| nonce_id(&first.nonce) == nonce)?;
        let phase = first
            .normalizing
            .output
            .as_ref()
            .map(|(output, _)| output.phase());
        match phase {
            Some(Phase::RejectPendingNotice(reason) | Phase::Rejected(reason)) => Some(reason),
            _ => first.normalizing.refused,
        }
    }
}

fn terminal(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Adopted | Phase::RejectPendingNotice(_) | Phase::Rejected(_)
    )
}
