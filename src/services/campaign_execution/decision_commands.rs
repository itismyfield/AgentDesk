use crate::domain::campaign::{
    decision_item::{ApprovalBinding, ApprovalError},
    definition::{DefinitionError, DefinitionErrorCode, DefinitionInput},
    model::{CampaignControl, CampaignState, Command, EventKind, Rejection, RejectionCode},
};
use chrono::{DateTime, Utc};

fn refused(error: ApprovalError) -> Rejection {
    Rejection::new(match error {
        ApprovalError::Invalid | ApprovalError::Encoding => RejectionCode::InvalidDefinition,
        ApprovalError::Stale => RejectionCode::StaleBinding,
        ApprovalError::Conflict => RejectionCode::IdempotencyConflict,
    })
}

pub(super) fn validate_edit_input(input: &DefinitionInput) -> Result<(), Rejection> {
    if input.workflow_options.is_none() {
        return Err(Rejection::invalid_definition(DefinitionError {
            code: DefinitionErrorCode::InvalidField,
            target: Some("workflow_options".into()),
            path: Vec::new(),
            pipeline_errors: Vec::new(),
        }));
    }
    Ok(())
}

fn context(state: &CampaignState, binding: &ApprovalBinding) -> Result<(), Rejection> {
    if state.definition_revision != binding.definition_revision {
        return Err(Rejection::new(RejectionCode::StaleDefinition));
    }
    let run = state
        .run
        .as_ref()
        .filter(|run| run.id == binding.run_id)
        .ok_or_else(|| Rejection::new(RejectionCode::StaleRun))?;
    if !state.nodes.contains_key(&binding.node_id)
        || !state
            .definition
            .input()
            .nodes
            .iter()
            .any(|n| n.id == binding.node_id)
    {
        return Err(Rejection::new(RejectionCode::StaleBinding));
    }
    if state.control != CampaignControl::Running || run.stop.is_some() {
        return Err(Rejection::new(RejectionCode::InvalidState));
    }
    if !state
        .definition
        .input()
        .effective_workflow_options()
        .required(binding.gate)
    {
        return Err(Rejection::new(RejectionCode::GateNotPassed));
    }
    Ok(())
}

pub(super) fn decide(
    next: &mut CampaignState,
    command: &Command,
    at: DateTime<Utc>,
) -> Result<Option<EventKind>, Rejection> {
    let binding = match command {
        Command::OpenApprovalGate { binding } | Command::ApproveGate { binding, .. } => binding,
        _ => return Err(Rejection::new(RejectionCode::InvalidState)),
    };
    context(next, binding)?;
    let run = next
        .run
        .as_mut()
        .ok_or_else(|| Rejection::new(RejectionCode::StaleRun))?;
    match command {
        Command::OpenApprovalGate { binding } => {
            if !run.approvals.open((**binding).clone()).map_err(refused)? {
                return Ok(None);
            }
            Ok(Some(EventKind::ApprovalGateOpened {
                binding: binding.clone(),
            }))
        }
        Command::ApproveGate { binding, evidence } => {
            if !run
                .approvals
                .approve(binding, (**evidence).clone(), at)
                .map_err(refused)?
            {
                return Ok(None);
            }
            let approval = run
                .approvals
                .items
                .get(&binding.id().map_err(refused)?)
                .and_then(|item| item.approval.clone())
                .ok_or_else(|| Rejection::new(RejectionCode::InvalidState))?;
            Ok(Some(EventKind::ApprovalGateApproved {
                binding: binding.clone(),
                approval: Box::new(approval),
            }))
        }
        _ => Err(Rejection::new(RejectionCode::InvalidState)),
    }
}

#[cfg(test)]
#[path = "decision_commands_tests.rs"]
mod tests;
