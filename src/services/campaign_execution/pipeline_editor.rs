use crate::domain::campaign::{
    definition::{self, DefinitionError, DefinitionErrorCode},
    model::{
        ActionState, CampaignControl, CampaignState, EventKind, NodeDisposition, Rejection,
        RejectionCode,
    },
    pipeline::PipelineDefinition,
    pipeline_template::development_template,
    pipeline_validate::PipelineDiagnostic,
};

use super::control;

pub(crate) fn check_references(
    state: &CampaignState,
    candidate: &PipelineDefinition,
) -> Vec<PipelineDiagnostic> {
    let Some(run) = &state.run else {
        return Vec::new();
    };
    let saved = state
        .definition
        .input()
        .pipeline
        .as_deref()
        .map(|pipeline| {
            pipeline
                .states
                .iter()
                .map(|state| state.id.as_str())
                .collect::<std::collections::HashSet<_>>()
        });
    let unsettled_attempts = run
        .attempts
        .values()
        .filter(|attempt| attempt.outcome.is_none());
    let unsettled_actions = run.actions.values().filter(|action| {
        !matches!(
            action.state,
            ActionState::Reconciled | ActionState::Cancelled
        )
    });
    let recovery_attempts = run.fences.values().filter_map(|fence| {
        run.attempts
            .get(fence.interruption.as_ref()?.attempt_id.as_ref()?)
    });
    let mut occupied: std::collections::BTreeSet<_> = state
        .nodes
        .values()
        .map(|node| node.stage)
        .chain(
            unsettled_attempts
                .chain(recovery_attempts)
                .map(|attempt| attempt.binding.stage),
        )
        .chain(unsettled_actions.map(|action| action.binding.stage))
        .map(|stage| stage.pipeline_state_id())
        .collect();
    if state
        .nodes
        .values()
        .any(|node| node.disposition == NodeDisposition::Failed)
    {
        occupied.insert("failed");
    }
    let retained: std::collections::HashSet<_> = candidate
        .states
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    occupied
        .into_iter()
        .filter(|id| !retained.contains(id))
        .filter(|id| saved.as_ref().is_none_or(|ids| ids.contains(id)))
        .map(|id| PipelineDiagnostic::new("STATE_IN_USE", "/states", Some(id), None))
        .collect()
}

pub(super) fn require_references(
    state: &CampaignState,
    candidate: &PipelineDefinition,
) -> Result<(), Rejection> {
    let errors = check_references(state, candidate);
    if errors.is_empty() {
        return Ok(());
    }
    let mut rejection = Rejection::invalid_definition(DefinitionError {
        code: DefinitionErrorCode::InvalidPipeline,
        target: errors[0].state_id.clone(),
        path: Vec::new(),
        pipeline_errors: errors,
    });
    rejection.code = RejectionCode::StateInUse;
    Err(rejection)
}

pub(crate) fn migration_available(state: &CampaignState) -> bool {
    state.definition.input().pipeline.is_none()
        && (state.run.is_none() || state.control == CampaignControl::Stopped)
        && control::edit_guard(state).is_ok()
}

pub(super) fn save(
    state: &mut CampaignState,
    expected: u64,
    pipeline: Option<&PipelineDefinition>,
) -> Result<Option<EventKind>, Rejection> {
    control::revision(state, expected)?;
    control::edit_guard(state)?;
    let migrated = pipeline.is_none();
    let candidate = if let Some(pipeline) = pipeline {
        Box::new(pipeline.clone())
    } else {
        if state.definition.input().pipeline.is_some() {
            return Ok(None);
        }
        if !migration_available(state) {
            return Err(Rejection::new(RejectionCode::InvalidState));
        }
        Box::new(development_template())
    };
    definition::validate_pipeline(&candidate).map_err(Rejection::invalid_definition)?;
    require_references(state, &candidate)?;
    state.definition.set_pipeline(candidate);
    state.definition_revision += 1;
    Ok(Some(EventKind::PipelineSaved { migrated }))
}
