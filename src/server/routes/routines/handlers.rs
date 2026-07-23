use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde_json::{Value, json};

use crate::api_caller_observability::RequestPrincipal;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::routines::{
    NewRoutine, RoutineLifecycleEvent, RoutineScriptLoader, RoutineSessionCommand,
    execute_claimed_script_run, is_migrated_launchd_script_ref,
    validate_migrated_launchd_activation,
};
use crate::utils::api::clamp_api_limit;

use super::super::AppState;
use super::audit::audit_routine_delete;
use super::helpers::{
    ensure_routine_runtime_runnable, fallback_name, initial_attach_status,
    migrated_launchd_metadata_for_state, normalize_script_ref, routine_agent_executor,
    routine_discord_logger, routine_session_controller, routine_store, validate_agent_id_request,
    validate_distinct_fallback_agent, validate_execution_strategy_request,
    validate_max_retries_request, validate_run_status_filter, validate_schedule_request,
    validate_timeout_request,
};
use super::responses::{delete_routine_response, session_control_error, store_error};
use super::{
    AttachRoutineBody, ListRoutinesQuery, ListRunsQuery, PatchRoutineBody, ResumeRoutineBody,
    RoutineMetricsQuery, SearchRoutineRunResultsQuery,
};

pub async fn list_routines(
    State(state): State<AppState>,
    Query(query): Query<ListRoutinesQuery>,
) -> AppResult<Json<Value>> {
    let store = routine_store(&state)?;
    let routines = store
        .list_routines(query.agent_id.as_deref(), query.status.as_deref())
        .await
        .map_err(store_error)?;
    Ok(Json(json!({
        "routines": routines,
        "default_timeout_secs": state.config.routines.agent_timeout_secs,
    })))
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
    Ok(Json(json!({
        "routine": routine,
        "default_timeout_secs": state.config.routines.agent_timeout_secs,
    })))
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
    validate_max_retries_request(body.max_retries)?;
    let agent_id = validate_agent_id_request(&state, "agent_id", body.agent_id.as_deref()).await?;
    let fallback_agent_id = validate_agent_id_request(
        &state,
        "fallback_agent_id",
        body.fallback_agent_id.as_deref(),
    )
    .await?;
    validate_distinct_fallback_agent(agent_id.as_deref(), fallback_agent_id.as_deref())?;
    let initial_status = initial_attach_status(&script_ref).to_string();
    let routine = store
        .attach_routine(NewRoutine {
            agent_id,
            fallback_agent_id,
            max_retries: body.max_retries,
            script_ref,
            name,
            status: Some(initial_status),
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
    validate_max_retries_request(body.max_retries)?;
    if let Some(fallback_agent_id) = body.fallback_agent_id.as_present() {
        let fallback_agent_id =
            validate_agent_id_request(&state, "fallback_agent_id", fallback_agent_id.as_deref())
                .await?;
        let current = store
            .get_routine(&routine_id)
            .await
            .map_err(store_error)?
            .ok_or_else(|| AppError::not_found(format!("routine {routine_id} not found")))?;
        validate_distinct_fallback_agent(
            current.agent_id.as_deref(),
            fallback_agent_id.as_deref(),
        )?;
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
    let Some(routine) = store.get_routine(&routine_id).await.map_err(store_error)? else {
        return Err(AppError::not_found(format!(
            "paused routine {routine_id} not found"
        )));
    };
    if routine.status != "paused" {
        return Err(AppError::not_found(format!(
            "paused routine {routine_id} not found"
        )));
    }
    let metadata = migrated_launchd_metadata_for_state(&state, &routine.script_ref).await?;
    let routine_script_dirs = state.config.routines.script_dirs();
    validate_migrated_launchd_activation(
        &routine.script_ref,
        routine.checkpoint.as_ref(),
        metadata.as_ref(),
        &routine_script_dirs,
    )?;
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

pub async fn delete_routine(
    State(state): State<AppState>,
    Path(routine_id): Path<String>,
    headers: HeaderMap,
    principal: Option<Extension<RequestPrincipal>>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let store = routine_store(&state)?;
    let caller_agent_id =
        crate::services::kanban::resolve_requesting_agent_id_with_pg(store.pool(), &headers).await;
    let result = store
        .delete_detached_routine(
            &routine_id,
            caller_agent_id.as_deref(),
            principal.as_ref().map(|Extension(principal)| principal),
        )
        .await
        .map_err(store_error)?;
    audit_routine_delete(&routine_id, &result);
    delete_routine_response(&routine_id, result)
}

pub async fn run_routine_now(
    State(state): State<AppState>,
    Path(routine_id): Path<String>,
) -> AppResult<Json<Value>> {
    ensure_routine_runtime_runnable(&state.config.routines)?;

    let store = routine_store(&state)?;
    let Some(routine) = store.get_routine(&routine_id).await.map_err(store_error)? else {
        return Err(AppError::not_found(format!(
            "routine {routine_id} not found"
        )));
    };

    let routine_script_dirs = state.config.routines.script_dirs();
    let script_dirs_for_task = routine_script_dirs.clone();
    let requested_script_ref = routine.script_ref.clone();
    let script_ref_for_task = requested_script_ref.clone();
    let (loader, script) = tokio::task::spawn_blocking(move || {
        let loader = RoutineScriptLoader::new()
            .map_err(|error| format!("routine script loader init failed: {error}"))?;
        loader
            .load_dirs(&script_dirs_for_task)
            .map_err(|error| format!("routine script registry load failed: {error}"))?;
        let script = loader
            .get_script(&script_ref_for_task)
            .map_err(|error| format!("routine script lookup failed: {error}"))?;
        Ok::<_, String>((loader, script))
    })
    .await
    .map_err(|error| {
        AppError::internal(format!(
            "routine script registry blocking task failed: {error}"
        ))
        .with_code(ErrorCode::Internal)
    })?
    .map_err(|error| AppError::internal(error).with_code(ErrorCode::Config))?;
    let metadata = if is_migrated_launchd_script_ref(&requested_script_ref) {
        let Some(script) = script else {
            return Err(AppError::conflict(format!(
                "migrated routine {requested_script_ref} is invalid: routine script not loaded"
            )));
        };
        Some(script.metadata)
    } else {
        None
    };
    validate_migrated_launchd_activation(
        &routine.script_ref,
        routine.checkpoint.as_ref(),
        metadata.as_ref(),
        &routine_script_dirs,
    )?;

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
    let pause_on_terminal_failure = state.config.routines.failure_pause_auto_resume_secs > 0;
    let Some(outcome) = execute_claimed_script_run(
        &store,
        &loader,
        Some(&agent_executor),
        Some(&discord_logger),
        claimed,
        pause_on_terminal_failure,
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
