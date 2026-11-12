use super::types::{Feature, OffReason, Scope};
use crate::config::decision_gate::{DecisionGateConfig, normalize_repository};
use std::time::Duration;

pub(super) fn resolve(config: &DecisionGateConfig, scope: &Scope) -> Result<Duration, OffReason> {
    if !config.enabled {
        return Err(OffReason::GlobalDisabled);
    }
    let (enabled, deadline_ms) = match scope.feature {
        Feature::IssueDuplicate => (
            config.features.issue_duplicate.enabled,
            config.features.issue_duplicate.deadline_ms,
        ),
        Feature::HostAssignment => (
            config.features.host_assignment.enabled,
            config.features.host_assignment.deadline_ms,
        ),
        Feature::RoutinePrecheck => (
            config.features.routine_precheck.enabled,
            config.features.routine_precheck.deadline_ms,
        ),
    };
    if !enabled {
        return Err(OffReason::FeatureDisabled);
    }
    if let Some(repo) = &scope.repository {
        let enabled = normalize_repository(repo)
            .and_then(|repo| config.repositories.get(&repo))
            .is_some_and(|policy| policy.enabled);
        if !enabled {
            return Err(OffReason::RepositoryDisabled);
        }
    }
    Ok(Duration::from_millis(deadline_ms))
}
