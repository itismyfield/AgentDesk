use super::*;

#[derive(Clone, Copy)]
enum LifecycleAction {
    Pause,
    Resume,
    End,
}

impl LifecycleAction {
    fn command_name(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::End => "end",
        }
    }

    fn target_status(self) -> &'static str {
        match self {
            Self::Pause => "paused",
            Self::Resume => "active",
            Self::End => "completed",
        }
    }
}

pub(super) fn validate_patch_status(body: &UpdateRunBody) -> AppResult<()> {
    // AC4's DB CHECK is outside this migration-free round; keep the API-side
    // allow-list explicit rather than implying database enforcement exists.
    if body
        .status
        .as_deref()
        .is_some_and(|status| status != "active")
    {
        return Err(auto_queue_json_error(
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": "PATCH status only supports starting a pending run; use the pause, resume, or end endpoint for lifecycle transitions"}),
            ),
        ));
    }
    Ok(())
}

async fn run_lifecycle_command(
    state: State<AppState>,
    run_id: String,
    action: LifecycleAction,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(auto_queue_tuple_error(pg_unavailable_response()));
    };

    let changed = match action {
        LifecycleAction::Pause => crate::db::auto_queue::pause_run_on_pg(pool, &run_id).await,
        LifecycleAction::Resume => crate::db::auto_queue::resume_run_on_pg(pool, &run_id).await,
        LifecycleAction::End => {
            crate::services::auto_queue::cancel_run::end_run_with_pg(
                state.health_registry.clone(),
                pool,
                &run_id,
            )
            .await
        }
    }
    .map_err(|error| {
        auto_queue_json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": format!(
                    "{} auto-queue run '{run_id}': {error}",
                    action.command_name()
                )
            })),
        )
    })?;

    if !changed {
        let current_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
                .bind(&run_id)
                .fetch_optional(pool)
                .await
                .map_err(|error| {
                    auto_queue_json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("load auto-queue run '{run_id}': {error}")})),
                    )
                })?;

        let Some(status) = current_status else {
            return Err(auto_queue_json_error(
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("auto-queue run '{run_id}' not found")})),
            ));
        };

        if matches!(action, LifecycleAction::Resume) && status == "paused" {
            let blocked = crate::db::auto_queue::run_has_blocking_phase_gate_pg(pool, &run_id)
                .await
                .map_err(|error| {
                    auto_queue_json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("check blocking phase gates for run '{run_id}': {error}")})),
                    )
                })?;
            if blocked {
                // Match the global Resume contract: blocked runs stay paused,
                // return 200, and are reported as non-resumable.
                return Ok((
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "resumed_runs": 0,
                        "blocked_runs": 1,
                        "message": "No resumable runs",
                    })),
                ));
            }
        }

        return Err(auto_queue_json_error(
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "cannot {} auto-queue run '{run_id}' from status '{status}'",
                    action.command_name()
                ),
                "run_id": run_id,
                "status": status,
            })),
        ));
    }

    Ok((
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "run_id": run_id,
            "status": action.target_status(),
        })),
    ))
}

/// POST /api/queue/runs/{id}/pause
pub async fn pause_run(
    state: State<AppState>,
    Path(run_id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    run_lifecycle_command(state, run_id, LifecycleAction::Pause).await
}

/// POST /api/queue/runs/{id}/resume
pub async fn resume_run_scoped(
    state: State<AppState>,
    Path(run_id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    run_lifecycle_command(state, run_id, LifecycleAction::Resume).await
}

/// POST /api/queue/runs/{id}/end
pub async fn end_run(
    state: State<AppState>,
    Path(run_id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    run_lifecycle_command(state, run_id, LifecycleAction::End).await
}
