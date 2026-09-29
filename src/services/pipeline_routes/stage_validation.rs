use super::{
    PipelineRouteError, PipelineStageInput, StoredStage, normalize_optional, validate_backoff,
    validate_on_failure,
};
use std::collections::HashSet;

pub(super) fn validate_pipeline_stages(
    stages: &[PipelineStageInput],
) -> Result<(), PipelineRouteError> {
    let mut names = HashSet::new();
    let mut orders = HashSet::new();
    for (index, stage) in stages.iter().enumerate() {
        if !names.insert(stage.stage_name.as_str()) {
            return Err(PipelineRouteError::BadRequest {
                stage: stage.stage_name.clone(),
                error: "stage names must be unique within a repo".to_string(),
            });
        }
        let order = stage.stage_order.unwrap_or(index as i64 + 1);
        if !orders.insert(order) {
            return Err(PipelineRouteError::BadRequest {
                stage: stage.stage_name.clone(),
                error: "stage_order values must be unique within a repo".to_string(),
            });
        }
        if let Err(error) = validate_on_failure(stage.on_failure.as_deref()) {
            return Err(PipelineRouteError::BadRequest {
                stage: stage.stage_name.clone(),
                error,
            });
        }
        // Validate the same normalized backoff value that the save writes.
        if let Err(error) = validate_backoff(normalize_optional(stage.backoff.as_deref())) {
            return Err(PipelineRouteError::BadRequest {
                stage: stage.stage_name.clone(),
                error,
            });
        }
        if let Some(max_retries) = stage.max_retries
            && max_retries < 0
        {
            return Err(PipelineRouteError::BadRequest {
                stage: stage.stage_name.clone(),
                error: format!("max_retries={max_retries} must be >= 0"),
            });
        }
    }
    Ok(())
}

pub(super) fn validate_supported_stage_changes(
    stages: &[PipelineStageInput],
    stored: &[StoredStage],
) -> Result<(), PipelineRouteError> {
    for stage in stages {
        let kept = stored
            .iter()
            .find(|row| row.stage_name.as_deref() == Some(stage.stage_name.as_str()));
        // Preserve existing unsupported settings without enabling new ones.
        // A counter stage cannot apply an edited agent override.
        if stage.provider.as_deref().map(str::trim) == Some("counter")
            && !kept.is_some_and(|row| {
                row.provider == stage.provider && row.agent_override_id == stage.agent_override_id
            })
        {
            return Err(PipelineRouteError::BadRequest {
                stage: stage.stage_name.clone(),
                error: "counter stages are unsupported; only an existing provider and agent override may be preserved".to_string(),
            });
        }
        if normalize_optional(stage.skip_condition.as_deref()).is_some()
            && !kept.is_some_and(|row| row.skip_condition == stage.skip_condition)
        {
            return Err(PipelineRouteError::BadRequest {
                stage: stage.stage_name.clone(),
                error: "skip_condition is unsupported; only an existing condition may be preserved"
                    .to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "stage_validation_tests.rs"]
mod tests;
