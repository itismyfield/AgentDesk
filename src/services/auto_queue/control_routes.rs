/// PATCH /api/auto-queue/runs/{id}
pub async fn update_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRunBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body
        .deploy_phases
        .as_ref()
        .is_some_and(|phases| !phases.is_empty())
        && !deploy_phase_api_enabled(&state)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "deploy_phases requires server.auth_token to be configured"
            })),
        );
    }

    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    if let Some(max_concurrent_threads) = body.max_concurrent_threads {
        if max_concurrent_threads <= 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "max_concurrent_threads must be > 0"})),
            );
        }
    }

    let ignored_unified_thread = body.unified_thread.is_some();
    if body.status.is_none()
        && body.deploy_phases.is_none()
        && body.max_concurrent_threads.is_none()
        && !ignored_unified_thread
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    match update_run_with_pg(&id, &body, pool).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "ignored": ignored_unified_thread.then_some(vec!["unified_thread"]),
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// POST /api/auto-queue/slots/{agent_id}/{slot_index}/reset-thread
pub async fn reset_slot_thread(
    State(state): State<AppState>,
    Path((agent_id, slot_index)): Path<(String, i64)>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    match crate::services::auto_queue::runtime::reset_slot_thread_bindings_pg(
        pool, &agent_id, slot_index,
    )
    .await
    {
        Ok((archived_threads, cleared_sessions, cleared_bindings)) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "agent_id": agent_id,
                "slot_index": slot_index,
                "archived_threads": archived_threads,
                "cleared_sessions": cleared_sessions,
                "cleared_bindings": cleared_bindings,
            })),
        ),
        Err(err) if err.contains("has active dispatch") => {
            (StatusCode::CONFLICT, Json(json!({"error": err})))
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err})),
        ),
    }
}

/// POST /api/auto-queue/reset
/// Reset a single agent queue. Requires `agent_id`.
pub async fn reset(
    State(state): State<AppState>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let body: ResetBody = match parse_json_body(body, "reset") {
        Ok(parsed) => parsed,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };

    let agent_id = match body
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(agent_id) => agent_id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "agent_id is required for reset"})),
            );
        }
    };

    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    match reset_scoped_with_pg(agent_id, pool).await {
        Ok(response) => (StatusCode::OK, Json(response)),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// POST /api/auto-queue/reset-global
/// Global reset requires an explicit confirmation token.
pub async fn reset_global(
    State(state): State<AppState>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let body: ResetGlobalBody = match parse_json_body(body, "reset-global") {
        Ok(parsed) => parsed,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };

    let confirmation_token = body
        .confirmation_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if confirmation_token != Some(RESET_GLOBAL_CONFIRMATION_TOKEN) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "confirmation_token is required for reset-global"})),
        );
    }

    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    match reset_global_with_pg(pool).await {
        Ok(response) => (StatusCode::OK, Json(response)),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

/// POST /api/auto-queue/pause — soft-pause active runs; `force=true` keeps the legacy cancel path
pub async fn pause(
    State(state): State<AppState>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let body: PauseBody = match parse_json_body(body, "pause") {
        Ok(parsed) => parsed,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };
    let force = body.force.unwrap_or(false);

    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    match if force {
        force_pause_with_pg(state.health_registry.clone(), pool).await
    } else {
        soft_pause_with_pg(pool).await
    } {
        Ok(response) => (StatusCode::OK, Json(response)),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

fn cancel_route_error_response(
    error: crate::error::AppError,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut body = json!({ "error": error.message() });
    if let Some(run_id) = error.context().get("run_id") {
        body["run_id"] = run_id.clone();
    }
    if let Some(status) = error.context().get("status") {
        body["status"] = status.clone();
    }
    (error.status(), Json(body))
}

/// POST /api/auto-queue/resume — resume paused runs and dispatch next entry
pub async fn resume_run(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    let blocked_runs = match sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
         FROM auto_queue_runs r
         WHERE r.status = 'paused'
           AND EXISTS (
               SELECT 1
               FROM auto_queue_phase_gates pg
               WHERE pg.run_id = r.id
                 AND pg.status IN ('pending', 'failed')
           )",
    )
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("count postgres blocked auto-queue runs: {error}")})),
            );
        }
    };
    let resumed = match sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'active',
             completed_at = NULL
         WHERE status = 'paused'
           AND NOT EXISTS (
               SELECT 1
               FROM auto_queue_phase_gates pg
               WHERE pg.run_id = auto_queue_runs.id
                 AND pg.status IN ('pending', 'failed')
           )",
    )
    .execute(pool)
    .await
    {
        Ok(result) => result.rows_affected() as i64,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("resume postgres auto-queue runs: {error}")})),
            );
        }
    };

    if resumed > 0 {
        let (_status, body) = activate(
            State(state),
            Json(ActivateBody {
                run_id: None,
                repo: None,
                agent_id: None,
                thread_group: None,
                unified_thread: None,
                active_only: Some(true),
            }),
        )
        .await;
        let dispatched = body.0.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        return (
            StatusCode::OK,
            Json(
                json!({"ok": true, "resumed_runs": resumed, "blocked_runs": blocked_runs, "dispatched": dispatched}),
            ),
        );
    }

    (
        StatusCode::OK,
        Json(
            json!({"ok": true, "resumed_runs": 0, "blocked_runs": blocked_runs, "message": "No resumable runs"}),
        ),
    )
}

/// POST /api/auto-queue/cancel — cancel all active/paused runs and pending entries
pub async fn cancel(
    State(state): State<AppState>,
    Query(query): Query<CancelQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    let service = state.auto_queue_service();
    let result = if let Some(run_id) = query
        .run_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        service
            .cancel_run_with_pg(state.health_registry.clone(), pool, run_id)
            .await
    } else {
        service
            .cancel_runs_with_pg(state.health_registry.clone(), pool)
            .await
    };
    match result {
        Ok(payload) => (StatusCode::OK, Json(payload)),
        Err(error) => cancel_route_error_response(error),
    }
}

/// PATCH /api/auto-queue/reorder
pub async fn reorder(
    State(state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    match reorder_with_pg(&body, pool).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Err(error) if error.starts_with("not_found:") => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": error.trim_start_matches("not_found:")})),
        ),
        Err(error)
            if error == "orderedIds cannot be empty"
                || error == "no pending entries found for reorder scope"
                || error == "orderedIds do not match any pending entries in scope"
                || error == "replacement sequence exhausted"
                || error == "replacement sequence was not fully consumed" =>
        {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": error })))
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

