use axum::http::StatusCode;
use serde_json::Value;

use crate::config::Config;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::routines::{
    RoutineAgentExecutor, RoutineDiscordLogger, RoutineScriptLoader, RoutineSessionController,
    RoutineStore, is_migrated_launchd_script_ref, validate_routine_runtime_config,
    validate_routine_schedule,
};

use super::super::AppState;
use super::PARALLEL_SAFE_MIGRATED_LAUNCHD_SCRIPT_REF;

pub(super) fn ensure_routine_runtime_runnable(
    config: &crate::config::RoutinesConfig,
) -> AppResult<()> {
    if !config.enabled {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "routines are disabled by config",
        ));
    }
    if let Err(error) = validate_routine_runtime_config(config) {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            format!("routine runtime is not runnable: {}", error.message()),
        ));
    }
    Ok(())
}

pub(super) async fn migrated_launchd_metadata_for_state(
    state: &AppState,
    script_ref: &str,
) -> AppResult<Option<Value>> {
    if !is_migrated_launchd_script_ref(script_ref) {
        return Ok(None);
    }
    let routine_script_dirs = state.config.routines.script_dirs();
    let requested_script_ref = script_ref.to_string();
    let script_ref_for_task = requested_script_ref.clone();
    let script = tokio::task::spawn_blocking(move || {
        let loader = RoutineScriptLoader::new()
            .map_err(|error| format!("routine script loader init failed: {error}"))?;
        loader
            .load_dirs(&routine_script_dirs)
            .map_err(|error| format!("routine script registry load failed: {error}"))?;
        loader
            .get_script(&script_ref_for_task)
            .map_err(|error| format!("routine script lookup failed: {error}"))
    })
    .await
    .map_err(|error| {
        AppError::internal(format!(
            "routine script registry blocking task failed: {error}"
        ))
        .with_code(ErrorCode::Internal)
    })?
    .map_err(|error| AppError::internal(error).with_code(ErrorCode::Config))?;
    let Some(script) = script else {
        return Err(AppError::conflict(format!(
            "migrated routine {requested_script_ref} is invalid: routine script not loaded"
        )));
    };
    Ok(Some(script.metadata))
}

pub(super) fn routine_store(state: &AppState) -> AppResult<RoutineStore> {
    let Some(pool) = state.pg_pool.clone() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable; routines require postgresql",
        ));
    };
    Ok(RoutineStore::new_with_timezone_and_checkpoint_limit(
        std::sync::Arc::new(pool),
        state.config.routines.default_timezone.clone(),
        state.config.routines.max_checkpoint_bytes,
    ))
}

pub(super) fn routine_agent_executor(state: &AppState) -> AppResult<RoutineAgentExecutor> {
    let Some(pool) = state.pg_pool.clone() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable; routines require postgresql",
        ));
    };
    Ok(RoutineAgentExecutor::new(
        std::sync::Arc::new(pool),
        state.health_registry.clone(),
        state.config.routines.agent_timeout_secs,
    ))
}

pub(super) fn routine_discord_logger(state: &AppState) -> AppResult<RoutineDiscordLogger> {
    let Some(pool) = state.pg_pool.clone() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable; routines require postgresql",
        ));
    };
    Ok(RoutineDiscordLogger::new_with_health_registry(
        std::sync::Arc::new(pool),
        state.health_registry.clone(),
        routine_health_target(&state.config),
    ))
}

pub(super) fn routine_session_controller(state: &AppState) -> AppResult<RoutineSessionController> {
    let Some(pool) = state.pg_pool.clone() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable; routines require postgresql",
        ));
    };
    Ok(RoutineSessionController::new(
        std::sync::Arc::new(pool),
        state.health_registry.clone(),
    ))
}

pub(super) fn fallback_name(script_ref: &str) -> String {
    std::path::Path::new(script_ref)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

pub(super) fn initial_attach_status(script_ref: &str) -> &'static str {
    if script_ref == PARALLEL_SAFE_MIGRATED_LAUNCHD_SCRIPT_REF {
        "enabled"
    } else if is_migrated_launchd_script_ref(script_ref) {
        "paused"
    } else {
        "enabled"
    }
}

pub(super) fn normalize_script_ref(script_ref: &str) -> AppResult<String> {
    let normalized = script_ref.trim().replace('\\', "/");
    if normalized.is_empty() {
        return Err(AppError::bad_request("script_ref is required"));
    }
    Ok(normalized)
}

pub(super) fn validate_execution_strategy_request(strategy: &str) -> AppResult<()> {
    match strategy {
        "fresh" | "persistent" => Ok(()),
        other => Err(AppError::bad_request(format!(
            "unsupported routine execution_strategy '{other}'; expected fresh or persistent"
        ))),
    }
}

pub(super) fn validate_run_status_filter(status: &str) -> AppResult<()> {
    match status {
        "running" | "succeeded" | "failed" | "skipped" | "paused" | "interrupted" => Ok(()),
        other => Err(AppError::bad_request(format!(
            "unsupported routine run status '{other}'"
        ))),
    }
}

pub(super) fn validate_schedule_request(schedule: Option<&str>) -> AppResult<()> {
    let Some(schedule) = schedule else {
        return Ok(());
    };
    validate_routine_schedule(schedule).map_err(|error| AppError::bad_request(error.to_string()))
}

pub(super) fn validate_timeout_request(timeout_secs: Option<i32>) -> AppResult<()> {
    if matches!(timeout_secs, Some(value) if value <= 0) {
        return Err(AppError::bad_request(
            "routine timeout_secs must be greater than zero",
        ));
    }
    Ok(())
}

pub(super) fn validate_max_retries_request(max_retries: Option<i32>) -> AppResult<()> {
    if matches!(max_retries, Some(value) if value < 0) {
        return Err(AppError::bad_request(
            "routine max_retries must be greater than or equal to zero",
        ));
    }
    Ok(())
}

pub(super) async fn validate_agent_id_request(
    state: &AppState,
    field_name: &str,
    agent_id: Option<&str>,
) -> AppResult<Option<String>> {
    let Some(agent_id) = agent_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let Some(pool) = state.pg_pool.as_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable; routines require postgresql",
        ));
    };
    match crate::db::kanban_cards::resolve_existing_agent_id_with_pg(pool, agent_id).await {
        Some(existing) => Ok(Some(existing)),
        None => Err(AppError::bad_request(format!(
            "routine {field_name} references unknown agent '{agent_id}'"
        ))),
    }
}

pub(super) fn validate_distinct_fallback_agent(
    agent_id: Option<&str>,
    fallback_agent_id: Option<&str>,
) -> AppResult<()> {
    let Some(agent_id) = agent_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let Some(fallback_agent_id) = fallback_agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    if agent_id == fallback_agent_id {
        return Err(AppError::bad_request(
            "routine fallback_agent_id must differ from agent_id",
        ));
    }
    Ok(())
}

fn routine_health_target(config: &Config) -> Option<String> {
    config
        .kanban
        .human_alert_channel_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("channel:{value}"))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::super::PARALLEL_SAFE_MIGRATED_LAUNCHD_SCRIPT_REF;
    use super::{
        ensure_routine_runtime_runnable, initial_attach_status, normalize_script_ref,
        validate_distinct_fallback_agent,
    };
    use crate::config::RoutinesConfig;
    use crate::error::ErrorCode;

    #[test]
    fn run_now_guard_rejects_disabled_routines() {
        let config = RoutinesConfig::default();

        let err = ensure_routine_runtime_runnable(&config)
            .expect_err("disabled routines must reject run-now before DB access");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.code(), ErrorCode::Config);
        assert_eq!(err.message(), "routines are disabled by config");
    }

    #[test]
    fn fallback_agent_must_differ_from_primary_agent() {
        assert!(validate_distinct_fallback_agent(Some("codex"), Some("claude")).is_ok());
        assert!(validate_distinct_fallback_agent(None, Some("claude")).is_ok());
        assert!(validate_distinct_fallback_agent(Some("codex"), None).is_ok());

        let err = validate_distinct_fallback_agent(Some("codex"), Some("codex"))
            .expect_err("self fallback must be rejected");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn run_now_guard_rejects_invalid_runtime_worker_config() {
        let mut config = RoutinesConfig {
            enabled: true,
            ..RoutinesConfig::default()
        };
        config.max_agent_polls_per_tick = 0;

        let err = ensure_routine_runtime_runnable(&config)
            .expect_err("worker-invalid routines config must reject run-now");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.code(), ErrorCode::Config);
        assert!(
            err.message().contains("max_agent_polls_per_tick"),
            "unexpected message: {}",
            err.message()
        );
    }

    #[test]
    fn normalize_script_ref_trims_and_matches_loader_separator() {
        assert_eq!(
            normalize_script_ref(" nested\\summary.js \n").unwrap(),
            "nested/summary.js"
        );

        let err = normalize_script_ref(" \t ").expect_err("empty refs must be rejected");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.message(), "script_ref is required");
    }

    #[test]
    fn migrated_launchd_attach_defaults_to_paused() {
        assert_eq!(
            initial_attach_status("migrated-launchd/memory-merge.js"),
            "paused"
        );
        assert_eq!(
            initial_attach_status(PARALLEL_SAFE_MIGRATED_LAUNCHD_SCRIPT_REF),
            "enabled"
        );
        assert_eq!(
            initial_attach_status("monitoring/automation-candidate-detector.js"),
            "enabled"
        );
    }
}
