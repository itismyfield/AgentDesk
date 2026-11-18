use crate::domain::campaign::{
    lifecycle::{HoldReason, NextAction, StepKey},
    workflow_options::ApprovalGate,
};

use super::lifecycle::{self, GateStatus, GateTarget};
use super::model::*;

fn hold(reason: HoldReason) -> NextAction {
    NextAction::Hold { reason }
}

fn gate_wait(gate: ApprovalGate, status: GateStatus) -> Option<NextAction> {
    match status {
        GateStatus::Clear => None,
        GateStatus::Unopened => Some(NextAction::OpenGate { gate }),
        GateStatus::Unapproved => Some(NextAction::AwaitApproval { gate }),
    }
}

// The one next step of a node, from the saved state alone: recomputed after a restart it is the
// same step with the same key, and once that step is applied it is never proposed again.
pub fn next_action(state: &CampaignState, node_id: &str) -> NextAction {
    let (Some(run), Some(node)) = (state.run.as_ref(), state.nodes.get(node_id)) else {
        return hold(HoldReason::NotRunning);
    };
    if state.control != CampaignControl::Running || run.stop.is_some() {
        return hold(HoldReason::NotRunning);
    }
    match node.disposition {
        NodeDisposition::Failed => return hold(HoldReason::BudgetExhausted),
        NodeDisposition::Complete => return hold(HoldReason::Complete),
        _ => {}
    }
    let fence = run.fences.get(node_id).cloned().unwrap_or_default();
    if fence.interruption.is_some() {
        return hold(HoldReason::Interrupted);
    }
    let key = StepKey {
        run_id: run.id.clone(),
        node_id: node_id.into(),
        series: node.series,
        repair_cycle: node.repair_cycles,
        epoch: fence.epoch,
    };
    // No new start is proposed while any session or action of this run is unobserved.
    let unknown = run
        .sessions
        .iter()
        .any(|s| matches!(s.observation, SessionObservation::Unknown { .. }))
        || run.actions.values().any(|a| {
            a.binding.node_id == node_id
                && matches!(a.state, ActionState::StartPending | ActionState::Unknown)
        });
    let start = |step: NextAction| {
        if unknown {
            hold(HoldReason::ObservationUnknown)
        } else {
            step
        }
    };
    let open = node
        .current_attempt
        .as_ref()
        .and_then(|id| run.attempts.get(id))
        .filter(|a| a.outcome.is_none());
    let gate = |gate, target: Option<&GateTarget<'_>>| {
        gate_wait(
            gate,
            lifecycle::gate_status(state, run, node_id, gate, target),
        )
    };
    match (node.stage, open) {
        (Stage::Requirements | Stage::Implementation, None) => {
            if node.stage == Stage::Requirements
                && lifecycle::unmet_dependency(state, node_id).is_some()
            {
                return hold(HoldReason::PredecessorMain);
            }
            // The Work's candidate is the driver's observation; AssignWork matches it exactly.
            let waiting = lifecycle::work_gates(node.stage)
                .iter()
                .find_map(|slot| gate(*slot, None));
            waiting.unwrap_or_else(|| start(NextAction::AssignWork { key }))
        }
        (Stage::Implementation, Some(work)) => {
            let work_attempt = work.binding.attempt_id.clone();
            let Some(seal) = run.seals.get(&work_attempt).filter(|_| work.started) else {
                return NextAction::AwaitAttempt {
                    attempt_id: work_attempt,
                };
            };
            let candidate = lifecycle::reviewed(seal);
            gate(ApprovalGate::Review, Some(&GateTarget::of(&candidate)))
                .unwrap_or_else(|| start(NextAction::BeginReview { key, work_attempt }))
        }
        (Stage::Review, Some(review)) => NextAction::AwaitAttempt {
            attempt_id: review.binding.attempt_id.clone(),
        },
        (Stage::Landing, _) if node.disposition == NodeDisposition::Pending => {
            let Some(candidate) = lifecycle::landing_candidate(state, run, node_id) else {
                return hold(HoldReason::NeedsInspection);
            };
            gate(ApprovalGate::Landing, Some(&GateTarget::of(candidate)))
                .unwrap_or(NextAction::BeginLanding { key })
        }
        (Stage::Landing | Stage::MainVerification | Stage::FinalVerification, _) => {
            if lifecycle::confirmed(node) && lifecycle::settled(run, node_id) {
                NextAction::CompleteNode { key }
            } else {
                NextAction::AwaitEvidence { stage: node.stage }
            }
        }
        (Stage::Complete, _) => hold(HoldReason::Complete),
        (Stage::Design | Stage::DesignReview, _) => hold(HoldReason::Unsupported),
        // A passed Work without a Review exists only after a finishing stop; resume re-enters Work.
        (Stage::Review, None) => hold(HoldReason::NeedsInspection),
        (Stage::Requirements, Some(_)) => hold(HoldReason::NeedsInspection),
    }
}

#[cfg(test)]
#[path = "scheduler_tests.rs"]
mod tests;
