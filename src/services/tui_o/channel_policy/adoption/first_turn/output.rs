//! The admitted first output's one normalization, kept with its admission under the adoption
//! lock so every body claim of that turn reads the same terminal answer.

use super::*;
use crate::services::tui_o::channel_policy::output_disposition::{
    BUDGET, Identity, Output, Phase, Reason,
};
use std::sync::{Arc, atomic::AtomicBool};
use tokio::time::Instant;

#[derive(Clone, Debug)]
pub(super) struct Normalizing {
    admitted_at: Instant,
    /// The admission's one repair, trust or binding, spent at most once across every view.
    repair: Arc<AtomicBool>,
    /// The disposition and the proof it was made from, once a bound rollout proved the admission.
    output: Option<(Output, FirstProof)>,
    /// Why the admission was refused when no rollout proved it within the budget.
    refused: Option<Reason>,
}

impl Normalizing {
    pub(super) fn admitted() -> Self {
        Self {
            admitted_at: Instant::now(),
            repair: Arc::default(),
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
                let n = &first.normalizing;
                let output = Output::admitted(id, n.admitted_at, Arc::clone(&n.repair));
                first.normalizing.output = Some((output, proof));
                true
            });
        }
        drop(guard);
    }

    /// The admission `context` prepared, before any rollout proves it: a view that spends the
    /// admission's own deadline and repair. None once proved, ended, refused or repaired.
    pub(crate) fn first_trust_window(&self, context: &BindingContext) -> Option<Output> {
        let first = self.first_turn.borrow();
        let first = first.as_ref().filter(|first| {
            let n = &first.normalizing;
            first.valid
                && first.pending
                && first.context.as_ref() == Some(context)
                && n.output.is_none()
                && n.refused.is_none()
                && !n.repair.load(std::sync::atomic::Ordering::SeqCst)
        })?;
        let id = Identity {
            channel: first.channel,
            input: first.mark.order,
            nonce: digest(&first.nonce),
            execution: digest(&context.execution_nonce),
            source: String::new(),
        };
        let n = &first.normalizing;
        Some(Output::admitted(id, n.admitted_at, Arc::clone(&n.repair)))
    }

    /// The pending, unproved admission launched in `tmux`: its execution and a view sharing its
    /// deadline and its one repair.
    pub(crate) fn first_trust_launch(&self, tmux: &str) -> Option<(BindingContext, Output)> {
        let first = self.first_turn.borrow();
        let first = first.as_ref().filter(|first| {
            let n = &first.normalizing;
            first.valid
                && first.pending
                && first.tmux == tmux
                && n.output.is_none()
                && n.refused.is_none()
        })?;
        let context = first.context.clone()?;
        let id = Identity {
            channel: first.channel,
            input: first.mark.order,
            nonce: digest(&first.nonce),
            execution: digest(&context.execution_nonce),
            source: String::new(),
        };
        let n = &first.normalizing;
        let output = Output::admitted(id, n.admitted_at, Arc::clone(&n.repair));
        Some((context, output))
    }

    /// What a body claim of the turn `nonce` names waits on: nothing once its output is adopted
    /// or rejected, or when it is not the admitted first turn.
    pub(crate) fn first_hold(&self, nonce: [u8; 32]) -> Option<FirstHold> {
        // The watch guard is released before the output's own lock is taken: a rejection holds
        // that lock while it invalidates the admission through this watch.
        let (output, pending, admitted_at) = {
            let first = self.first_turn.borrow();
            let first = first.as_ref().filter(|first| {
                nonce_id(&first.nonce) == nonce && first.normalizing.refused.is_none()
            })?;
            let n = &first.normalizing;
            (n.output.clone(), first.pending, n.admitted_at)
        };
        match output {
            Some((output, _)) if terminal(output.phase()) => None,
            Some(held) => Some(FirstHold::Output(Box::new(held))),
            None if pending => Some(FirstHold::Until(admitted_at + BUDGET)),
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
        // As in `first_hold`: copied under the watch guard, judged after it is released.
        let (output, refused) = {
            let first = self.first_turn.borrow();
            let first = first
                .as_ref()
                .filter(|first| nonce_id(&first.nonce) == nonce)?;
            let n = &first.normalizing;
            (
                n.output.as_ref().map(|(output, _)| output.clone()),
                n.refused,
            )
        };
        match output.map(|output| output.phase()) {
            Some(Phase::RejectPendingNotice(reason) | Phase::Rejected(reason)) => Some(reason),
            _ => refused,
        }
    }
}

fn terminal(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Adopted | Phase::RejectPendingNotice(_) | Phase::Rejected(_)
    )
}
