use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::domain::campaign::{
    decision_item::ApprovalBinding, lifecycle::NextAction, model::*, workflow_options::ApprovalGate,
};

use super::super::{control, scheduler};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanStatus {
    DecisionRequired,
    AwaitingApproval,
    AwaitingDeploy,
    AwaitingStart,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PendingGate<'a> {
    pub gate: ApprovalGate,
    pub binding: Option<&'a ApprovalBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodeRuntimeView<'a> {
    #[serde(flatten)]
    pub state: &'a NodeRuntime,
    pub pending_gate: Option<PendingGate<'a>>,
    pub wait_started_at: Option<DateTime<Utc>>,
    pub evidence_url: Option<&'a str>,
    pub reminder_interval_seconds: Option<u64>,
    pub human_status: Option<HumanStatus>,
}

impl<'a> NodeRuntimeView<'a> {
    pub(super) fn new(state: &'a CampaignState, node_id: &str, runtime: &'a NodeRuntime) -> Self {
        let mut projection = Self {
            state: runtime,
            pending_gate: None,
            wait_started_at: None,
            evidence_url: None,
            reminder_interval_seconds: None,
            human_status: None,
        };
        match scheduler::next_action(state, node_id) {
            NextAction::OpenGate { gate } => {
                projection.pending_gate = Some(PendingGate {
                    gate,
                    binding: None,
                });
                projection.human_status = Some(HumanStatus::DecisionRequired);
            }
            NextAction::AwaitApproval { gate } => {
                projection.pending_gate = Some(PendingGate {
                    gate,
                    binding: current_binding(state, node_id, gate),
                });
                projection.human_status = Some(HumanStatus::AwaitingApproval);
            }
            NextAction::AssignWork { .. } => {
                projection.human_status = Some(HumanStatus::AwaitingStart);
            }
            NextAction::AwaitAttempt { .. } | NextAction::AwaitEvidence { .. }
                if deploy_queued(state, node_id) =>
            {
                projection.human_status = Some(HumanStatus::AwaitingDeploy);
            }
            _ => {}
        }
        projection
    }
}

fn current_binding<'a>(
    state: &'a CampaignState,
    node_id: &str,
    gate: ApprovalGate,
) -> Option<&'a ApprovalBinding> {
    let run = state.run.as_ref()?;
    let book = &run.approvals;
    book.current
        .values()
        .filter_map(|id| book.items.get(id))
        .find(|item| {
            let binding = &item.binding;
            binding.node_id == node_id
                && binding.gate == gate
                && binding.run_id == run.id
                && binding.definition_revision == state.legacy_execution_revision()
                && item.approval.is_none()
        })
        .map(|item| &item.binding)
}

fn deploy_queued(state: &CampaignState, node_id: &str) -> bool {
    state
        .run
        .iter()
        .flat_map(|run| run.actions.values())
        .any(|action| {
            action.binding.node_id == node_id
                && action.kind == ActionKind::Deploy
                && matches!(action.state, ActionState::Planned | ActionState::Claimed)
                && control::validate_new_start(state, &action.binding).is_ok()
        })
}
