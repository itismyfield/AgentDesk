use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::config::Config;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::routines::{
    NewRoutine, RoutineAgentExecutor, RoutineDiscordLogger, RoutineLifecycleEvent, RoutinePatch,
    RoutineScriptLoader, RoutineSessionCommand, RoutineSessionController, RoutineStore,
    execute_claimed_script_run, validate_routine_runtime_config, validate_routine_schedule,
};
use crate::utils::api::clamp_api_limit;

use super::AppState;

#[derive(Debug, Deserialize)]
pub struct ListRoutinesQuery {
    pub agent_id: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListRunsQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct RoutineMetricsQuery {
    pub agent_id: Option<String>,
    pub since: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct SearchRoutineRunResultsQuery {
    pub q: String,
    pub agent_id: Option<String>,
    pub status: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct AttachRoutineBody {
    pub agent_id: Option<String>,
    pub script_ref: String,
    pub name: Option<String>,
    pub execution_strategy: Option<String>,
    pub schedule: Option<String>,
    pub next_due_at: Option<DateTime<Utc>>,
    pub checkpoint: Option<Value>,
    pub discord_thread_id: Option<String>,
    pub timeout_secs: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct PatchRoutineBody {
    pub name: Option<String>,
    pub execution_strategy: Option<String>,
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    schedule: PatchField<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    next_due_at: PatchField<Option<DateTime<Utc>>>,
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    checkpoint: PatchField<Option<Value>>,
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    discord_thread_id: PatchField<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    timeout_secs: PatchField<Option<i32>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResumeRoutineBody {
    /// PATCH semantics: only update `next_due_at` when the caller explicitly
    /// includes the field. A missing field preserves the existing value so a
    /// bare `{}` body never strands the routine by nulling `next_due_at`
    /// (#2395).
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    next_due_at: PatchField<Option<DateTime<Utc>>>,
}

impl ResumeRoutineBody {
    fn next_due_at_update(&self) -> Option<Option<DateTime<Utc>>> {
        match &self.next_due_at {
            PatchField::Missing => None,
            PatchField::Present(value) => Some(*value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatchField<T> {
    Missing,
    Present(T),
}

impl<T> Default for PatchField<T> {
    fn default() -> Self {
        Self::Missing
    }
}

impl<T> PatchField<T> {
    fn as_present(&self) -> Option<&T> {
        match self {
            Self::Missing => None,
            Self::Present(value) => Some(value),
        }
    }

    fn into_option(self) -> Option<T> {
        match self {
            Self::Missing => None,
            Self::Present(value) => Some(value),
        }
    }
}

fn deserialize_patch_field<'de, D, T>(deserializer: D) -> Result<PatchField<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(PatchField::Present)
}

impl PatchRoutineBody {
    fn into_patch(self) -> RoutinePatch {
        RoutinePatch {
            name: self.name,
            execution_strategy: self.execution_strategy,
            schedule: self.schedule.into_option(),
            next_due_at: self.next_due_at.into_option(),
            checkpoint: self.checkpoint.into_option(),
            discord_thread_id: self.discord_thread_id.into_option(),
            timeout_secs: self.timeout_secs.into_option(),
        }
    }
}

pub async fn list_routines(
    State(state): State<AppState>,
    Query(query): Query<ListRoutinesQuery>,
) -> AppResult<Json<Value>> {
    let store = routine_store(&state)?;
    let routines = store
        .list_routines(query.agent_id.as_deref(), query.status.as_deref())
        .await
        .map_err(store_error)?;
    Ok(Json(json!({ "routines": routines })))
}

pub async fn routine_metrics(
    State(state): State<AppState>,
    Query(query): Query<RoutineMetricsQuery>,
) -> AppResult<Json<Value>> {
    let store = routine_store(&state)?;
    let metrics = store
        .metrics(query.agent_id.as_deref(), query.since)
        .await
        .map_err(store_error)?;
    Ok(Json(json!({
        "metrics": metrics,
        "filters": {
            "agent_id": query.agent_id,
            "since": query.since,
        },
    })))
}

pub async fn search_routine_run_results(
    State(state): State<AppState>,
    Query(query): Query<SearchRoutineRunResultsQuery>,
) -> AppResult<Json<Value>> {
    let q = query.q.trim();
    if q.is_empty() {
        return Err(AppError::bad_request("q is required"));
    }
    if let Some(status) = query.status.as_deref() {
        validate_run_status_filter(status)?;
    }
    let store = routine_store(&state)?;
    let limit = clamp_api_limit(Some(query.limit.unwrap_or(20).max(0) as usize)) as i64;
    let runs = store
        .search_run_results(
            q,
            query.agent_id.as_deref(),
            query.status.as_deref(),
            query.since,
            limit,
        )
        .await
        .map_err(store_error)?;
    Ok(Json(json!({
        "runs": runs,
        "filters": {
            "q": q,
            "agent_id": query.agent_id,
            "status": query.status,
            "since": query.since,
            "limit": limit,
        },
    })))
}

pub async fn get_routine(
    State(state): State<AppState>,
    Path(routine_id): Path<String>,
) -> AppResult<Json<Value>> {
    let store = routine_store(&state)?;
    let Some(routine) = store.get_routine(&routine_id).await.map_err(store_error)? else {
        return Err(AppError::not_found(format!(
            "routine {routine_id} not found"
        )));
    };
    Ok(Json(json!({ "routine": routine })))
}

pub async fn list_routine_runs(
    State(state): State<AppState>,
    Path(routine_id): Path<String>,
    Query(query): Query<ListRunsQuery>,
) -> AppResult<Json<Value>> {
    let store = routine_store(&state)?;
    if store
        .get_routine(&routine_id)
        .await
        .map_err(store_error)?
        .is_none()
    {
        return Err(AppError::not_found(format!(
            "routine {routine_id} not found"
        )));
    }
    let runs = store
        .list_runs(&routine_id, query.limit.unwrap_or(20))
        .await
        .map_err(store_error)?;
    Ok(Json(json!({ "runs": runs })))
}

pub async fn attach_routine(
    State(state): State<AppState>,
    Json(body): Json<AttachRoutineBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let store = routine_store(&state)?;
    let script_ref = normalize_script_ref(&body.script_ref)?;
    let name = body.name.unwrap_or_else(|| fallback_name(&script_ref));
    let execution_strategy = body
        .execution_strategy
        .unwrap_or_else(|| "fresh".to_string());
    validate_execution_strategy_request(&execution_strategy)?;
    validate_schedule_request(body.schedule.as_deref())?;
    validate_timeout_request(body.timeout_secs)?;
    let routine = store
        .attach_routine(NewRoutine {
            agent_id: body.agent_id,
            script_ref,
            name,
            execution_strategy,
            schedule: body.schedule,
            next_due_at: body.next_due_at,
            checkpoint: body.checkpoint,
            discord_thread_id: body.discord_thread_id,
            timeout_secs: body.timeout_secs,
        })
        .await
        .map_err(store_error)?;
    let discord_log = routine_discord_logger(&state)?
        .log_routine_event_with_store(&store, &routine, RoutineLifecycleEvent::Attached)
        .await;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "routine": routine, "discord_log": discord_log })),
    ))
}

pub async fn patch_routine(
    State(state): State<AppState>,
    Path(routine_id): Path<String>,
    Json(body): Json<PatchRoutineBody>,
) -> AppResult<Json<Value>> {
    let store = routine_store(&state)?;
    if let Some(strategy) = body.execution_strategy.as_deref() {
        validate_execution_strategy_request(strategy)?;
    }
    if let Some(Some(schedule)) = body.schedule.as_present() {
        validate_schedule_request(Some(schedule))?;
    }
    if let Some(timeout_secs) = body.timeout_secs.as_present().copied().flatten() {
        validate_timeout_request(Some(timeout_secs))?;
    }
    let patch = body.into_patch();
    let Some(routine) = store
        .patch_routine(&routine_id, patch)
        .await
        .map_err(store_error)?
    else {
        return Err(AppError::not_found(format!(
            "routine {routine_id} not found"
        )));
    };
    Ok(Json(json!({ "routine": routine })))
}

pub async fn pause_routine(
    State(state): State<AppState>,
    Path(routine_id): Path<String>,
) -> AppResult<Json<Value>> {
    let store = routine_store(&state)?;
    let changed = store
        .pause_routine(&routine_id)
        .await
        .map_err(store_error)?;
    if !changed {
        return Err(AppError::not_found(format!(
            "enabled routine {routine_id} not found"
        )));
    }
    let discord_log = routine_discord_logger(&state)?
        .log_routine_event_by_id(&store, &routine_id, RoutineLifecycleEvent::Paused)
        .await;
    Ok(Json(
        json!({ "ok": true, "routine_id": routine_id, "discord_log": discord_log }),
    ))
}

pub async fn resume_routine(
    State(state): State<AppState>,
    Path(routine_id): Path<String>,
    Json(body): Json<ResumeRoutineBody>,
) -> AppResult<Json<Value>> {
    let store = routine_store(&state)?;
    let changed = store
        .resume_routine(&routine_id, body.next_due_at_update())
        .await
        .map_err(store_error)?;
    if !changed {
        return Err(AppError::not_found(format!(
            "paused routine {routine_id} not found"
        )));
    }
    let discord_log = routine_discord_logger(&state)?
        .log_routine_event_by_id(&store, &routine_id, RoutineLifecycleEvent::Resumed)
        .await;
    Ok(Json(
        json!({ "ok": true, "routine_id": routine_id, "discord_log": discord_log }),
    ))
}

pub async fn detach_routine(
    State(state): State<AppState>,
    Path(routine_id): Path<String>,
) -> AppResult<Json<Value>> {
    let store = routine_store(&state)?;
    let changed = store
        .detach_routine(&routine_id)
        .await
        .map_err(store_error)?;
    if !changed {
        return Err(AppError::conflict(format!(
            "routine {routine_id} is missing, already detached, or currently running"
        )));
    }
    let discord_log = routine_discord_logger(&state)?
        .log_routine_event_by_id(&store, &routine_id, RoutineLifecycleEvent::Detached)
        .await;
    Ok(Json(
        json!({ "ok": true, "routine_id": routine_id, "discord_log": discord_log }),
    ))
}

pub async fn run_routine_now(
    State(state): State<AppState>,
    Path(routine_id): Path<String>,
) -> AppResult<Json<Value>> {
    ensure_routine_runtime_runnable(&state.config.routines)?;

    let store = routine_store(&state)?;
    if store
        .get_routine(&routine_id)
        .await
        .map_err(store_error)?
        .is_none()
    {
        return Err(AppError::not_found(format!(
            "routine {routine_id} not found"
        )));
    }

    let loader = RoutineScriptLoader::new().map_err(|error| {
        AppError::internal(format!("routine script loader init failed: {error}"))
            .with_code(ErrorCode::Internal)
    })?;
    let routine_script_dirs = state.config.routines.script_dirs();
    loader.load_dirs(&routine_script_dirs).map_err(|error| {
        AppError::internal(format!("routine script registry load failed: {error}"))
            .with_code(ErrorCode::Config)
    })?;

    let Some(claimed) = store
        .claim_run_now(&routine_id)
        .await
        .map_err(store_error)?
    else {
        return Err(AppError::conflict(format!(
            "routine {routine_id} is not enabled or already running"
        )));
    };

    let agent_executor = routine_agent_executor(&state)?;
    let discord_logger = routine_discord_logger(&state)?;
    discord_logger.log_run_started(&store, &claimed).await;
    let run_id = claimed.run_id.clone();
    let Some(outcome) = execute_claimed_script_run(
        &store,
        &loader,
        Some(&agent_executor),
        Some(&discord_logger),
        claimed,
    )
    .await
    .map_err(store_error)?
    else {
        return Err(AppError::conflict(format!(
            "routine run {run_id} was already closed before outcome capture"
        )));
    };
    let discord_log = discord_logger.log_run_outcome(&store, &outcome).await;
    Ok(Json(
        json!({ "outcome": outcome, "discord_log": discord_log }),
    ))
}

fn ensure_routine_runtime_runnable(config: &crate::config::RoutinesConfig) -> AppResult<()> {
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

pub async fn reset_routine_session(
    State(state): State<AppState>,
    Path(routine_id): Path<String>,
) -> AppResult<Json<Value>> {
    control_routine_session(&state, routine_id, RoutineSessionCommand::Reset).await
}

pub async fn kill_routine_session(
    State(state): State<AppState>,
    Path(routine_id): Path<String>,
) -> AppResult<Json<Value>> {
    control_routine_session(&state, routine_id, RoutineSessionCommand::Kill).await
}

async fn control_routine_session(
    state: &AppState,
    routine_id: String,
    command: RoutineSessionCommand,
) -> AppResult<Json<Value>> {
    let store = routine_store(state)?;
    let Some(routine) = store.get_routine(&routine_id).await.map_err(store_error)? else {
        return Err(AppError::not_found(format!(
            "routine {routine_id} not found"
        )));
    };
    if routine.execution_strategy != "persistent" {
        return Err(AppError::conflict(format!(
            "routine {routine_id} uses execution_strategy={}; session control requires persistent",
            routine.execution_strategy
        )));
    }
    if routine
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        return Err(AppError::conflict(format!(
            "routine {routine_id} is not attached to an agent"
        )));
    }

    let reason = format!(
        "routine persistent session {} via POST /api/routines/{}/session/{}",
        command.as_str(),
        routine_id,
        command.as_str()
    );
    let session = routine_session_controller(state)?
        .control_persistent_session(&routine, command, &reason)
        .await
        .map_err(session_control_error)?;
    let session_changed = session.runtime_cleared
        || session.tmux_killed
        || session.inflight_cleared
        || session.disconnected_sessions > 0;
    let interrupted_run_id = if session_changed {
        store
            .interrupt_in_flight_run(
                &routine_id,
                &reason,
                Some(json!({
                    "status": "interrupted_by_session_control",
                    "routine_id": routine_id,
                    "action": command.as_str(),
                    "provider": session.provider.clone(),
                    "channel_id": session.channel_id.clone(),
                    "session_key": session.session_key.clone(),
                    "tmux_session": session.tmux_session.clone(),
                })),
            )
            .await
            .map_err(store_error)?
    } else {
        None
    };

    Ok(Json(json!({
        "ok": true,
        "session": session,
        "interrupted_run_id": interrupted_run_id,
    })))
}

fn routine_store(state: &AppState) -> AppResult<RoutineStore> {
    let Some(pool) = state.pg_pool.clone() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable; routines require postgresql",
        ));
    };
    Ok(RoutineStore::new_with_timezone(
        std::sync::Arc::new(pool),
        state.config.routines.default_timezone.clone(),
    ))
}

fn routine_agent_executor(state: &AppState) -> AppResult<RoutineAgentExecutor> {
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

fn routine_discord_logger(state: &AppState) -> AppResult<RoutineDiscordLogger> {
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

fn routine_session_controller(state: &AppState) -> AppResult<RoutineSessionController> {
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

fn fallback_name(script_ref: &str) -> String {
    std::path::Path::new(script_ref)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

fn normalize_script_ref(script_ref: &str) -> AppResult<String> {
    let normalized = script_ref.trim().replace('\\', "/");
    if normalized.is_empty() {
        return Err(AppError::bad_request("script_ref is required"));
    }
    Ok(normalized)
}

fn validate_execution_strategy_request(strategy: &str) -> AppResult<()> {
    match strategy {
        "fresh" | "persistent" => Ok(()),
        other => Err(AppError::bad_request(format!(
            "unsupported routine execution_strategy '{other}'; expected fresh or persistent"
        ))),
    }
}

fn validate_run_status_filter(status: &str) -> AppResult<()> {
    match status {
        "running" | "succeeded" | "failed" | "skipped" | "paused" | "interrupted" => Ok(()),
        other => Err(AppError::bad_request(format!(
            "unsupported routine run status '{other}'"
        ))),
    }
}

fn validate_schedule_request(schedule: Option<&str>) -> AppResult<()> {
    let Some(schedule) = schedule else {
        return Ok(());
    };
    validate_routine_schedule(schedule).map_err(|error| AppError::bad_request(error.to_string()))
}

fn validate_timeout_request(timeout_secs: Option<i32>) -> AppResult<()> {
    if matches!(timeout_secs, Some(value) if value <= 0) {
        return Err(AppError::bad_request(
            "routine timeout_secs must be greater than zero",
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

fn store_error(error: anyhow::Error) -> AppError {
    AppError::internal(error.to_string()).with_code(ErrorCode::Database)
}

fn session_control_error(error: anyhow::Error) -> AppError {
    let message = error.to_string();
    if message.contains("not found") {
        AppError::not_found(message)
    } else if message.contains("not configured")
        || message.contains("not attached")
        || message.contains("invalid")
        || message.contains("requires execution_strategy")
    {
        AppError::conflict(message)
    } else {
        AppError::internal(message)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use super::{
        PatchRoutineBody, ResumeRoutineBody, ensure_routine_runtime_runnable, normalize_script_ref,
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
    fn patch_body_preserves_omitted_nullable_fields() {
        let body: PatchRoutineBody = serde_json::from_value(json!({})).unwrap();
        let patch = body.into_patch();

        assert_eq!(patch.schedule, None);
        assert_eq!(patch.next_due_at, None);
        assert_eq!(patch.checkpoint, None);
        assert_eq!(patch.discord_thread_id, None);
        assert_eq!(patch.timeout_secs, None);
    }

    #[test]
    fn patch_body_preserves_explicit_null_nullable_fields() {
        let body: PatchRoutineBody = serde_json::from_value(json!({
            "schedule": null,
            "next_due_at": null,
            "checkpoint": null,
            "discord_thread_id": null,
            "timeout_secs": null
        }))
        .unwrap();
        let patch = body.into_patch();

        assert_eq!(patch.schedule, Some(None));
        assert_eq!(patch.next_due_at, Some(None));
        assert_eq!(patch.checkpoint, Some(None));
        assert_eq!(patch.discord_thread_id, Some(None));
        assert_eq!(patch.timeout_secs, Some(None));
    }

    #[test]
    fn patch_body_preserves_present_nullable_values() {
        let body: PatchRoutineBody = serde_json::from_value(json!({
            "schedule": "@every 1h",
            "next_due_at": "2026-04-29T00:00:00Z",
            "checkpoint": {"cursor": "abc"},
            "discord_thread_id": "1234567890",
            "timeout_secs": 60
        }))
        .unwrap();
        let patch = body.into_patch();

        assert_eq!(patch.schedule, Some(Some("@every 1h".to_string())));
        assert!(patch.next_due_at.flatten().is_some());
        assert_eq!(patch.checkpoint, Some(Some(json!({"cursor": "abc"}))));
        assert_eq!(
            patch.discord_thread_id,
            Some(Some("1234567890".to_string()))
        );
        assert_eq!(patch.timeout_secs, Some(Some(60)));
    }

    /// #2395 — `POST /api/routines/:id/resume` with an empty body must NOT
    /// touch `next_due_at`. Previously a `{}` body deserialized to
    /// `next_due_at: None` and the SQL UPDATE wrote `next_due_at = NULL`,
    /// stranding the routine until dcserver restart.
    #[test]
    fn resume_body_omitted_next_due_at_is_preserved() {
        let body: ResumeRoutineBody = serde_json::from_value(json!({})).unwrap();
        assert_eq!(
            body.next_due_at_update(),
            None,
            "missing next_due_at must map to None (no SQL SET) so the existing column value is preserved"
        );
    }

    /// #2395 — explicit `"next_due_at": null` is the documented way to clear
    /// the next-fire timestamp (manual-only routines), and must still be
    /// distinguishable from a missing field.
    #[test]
    fn resume_body_explicit_null_clears_next_due_at() {
        let body: ResumeRoutineBody = serde_json::from_value(json!({
            "next_due_at": null,
        }))
        .unwrap();
        assert_eq!(body.next_due_at_update(), Some(None));
    }

    /// #2395 — present timestamp flows through to the store as
    /// `Some(Some(ts))`, producing a real SQL `SET next_due_at = $1`.
    #[test]
    fn resume_body_present_next_due_at_is_applied() {
        let body: ResumeRoutineBody = serde_json::from_value(json!({
            "next_due_at": "2026-04-29T00:00:00Z",
        }))
        .unwrap();
        let update = body.next_due_at_update().expect("field must be present");
        let ts = update.expect("timestamp must be Some");
        assert_eq!(ts.to_rfc3339(), "2026-04-29T00:00:00+00:00");
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
}
