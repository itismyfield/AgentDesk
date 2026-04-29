/// POST /api/auto-queue/slots/{agent_id}/{slot_index}/rebind
async fn rebind_slot_with_pg(
    agent_id: &str,
    slot_index: i64,
    body: &RebindSlotBody,
    pool: &sqlx::PgPool,
) -> (StatusCode, Json<serde_json::Value>) {
    let run_id = body.run_id.trim();
    let run_status = match sqlx::query_scalar::<_, String>(
        "SELECT status
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    {
        Ok(status) => status,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("load auto-queue run '{run_id}': {error}")})),
            );
        }
    };
    match run_status.as_deref() {
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("auto-queue run '{run_id}' not found")})),
            );
        }
        Some("active") | Some("paused") => {}
        Some(status) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("slot rebind requires an active or paused run (status={status})"),
                    "run_id": run_id,
                    "status": status,
                })),
            );
        }
    }

    let slot_pool_size = match crate::db::auto_queue::run_slot_pool_size_pg(pool, run_id).await {
        Ok(size) => size,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres slot pool size for {run_id}: {error}")}),
                ),
            );
        }
    };
    if slot_index >= slot_pool_size {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "slot_index {} is outside the slot pool for run '{}' (size={})",
                    slot_index,
                    run_id,
                    slot_pool_size
                ),
            })),
        );
    }

    let current_binding = match sqlx::query(
        "SELECT assigned_run_id, assigned_thread_group
         FROM auto_queue_slots
         WHERE agent_id = $1
           AND slot_index = $2",
    )
    .bind(agent_id)
    .bind(slot_index)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load slot binding for {agent_id}:{slot_index}: {error}")}),
                ),
            );
        }
    };
    let current_binding = match current_binding {
        Some(row) => {
            let assigned_run_id = match row.try_get("assigned_run_id") {
                Ok(value) => value,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode slot assigned_run_id for {agent_id}:{slot_index}: {error}")}),
                        ),
                    );
                }
            };
            let assigned_group = match row.try_get("assigned_thread_group") {
                Ok(value) => value,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({"error": format!("decode slot assigned_thread_group for {agent_id}:{slot_index}: {error}")}),
                        ),
                    );
                }
            };
            Some((assigned_run_id, assigned_group))
        }
        None => None,
    };
    let same_binding = current_binding.as_ref().is_some_and(
        |(assigned_run_id, assigned_group): &(Option<String>, Option<i64>)| {
            assigned_run_id.as_deref() == Some(run_id)
                && assigned_group.unwrap_or_default() == body.thread_group
        },
    );
    if !same_binding {
        match crate::db::auto_queue::slot_has_active_dispatch_pg(pool, agent_id, slot_index).await {
            Ok(true) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": format!(
                            "slot {} for {} has an active dispatch; reset or complete it before rebind",
                            slot_index, agent_id
                        ),
                    })),
                );
            }
            Ok(false) => {}
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("inspect active dispatches for {agent_id}:{slot_index}: {error}")}),
                    ),
                );
            }
        }
    }

    let updated_entries = match crate::db::auto_queue::rebind_slot_for_group_agent_pg(
        pool,
        run_id,
        body.thread_group,
        agent_id,
        slot_index,
    )
    .await
    {
        Ok(updated_entries) => updated_entries,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "agent_id": agent_id,
            "slot_index": slot_index,
            "run_id": run_id,
            "thread_group": body.thread_group,
            "rebound": !same_binding,
            "updated_entries": updated_entries,
        })),
    )
}

pub async fn rebind_slot(
    State(state): State<AppState>,
    Path((agent_id, slot_index)): Path<(String, i64)>,
    Json(body): Json<RebindSlotBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if slot_index < 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "slot_index must be >= 0"})),
        );
    }
    if body.thread_group < 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "thread_group must be >= 0"})),
        );
    }
    let run_id = body.run_id.trim();
    if run_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "run_id is required"})),
        );
    }

    let Some(pg_pool) = state.pg_pool.clone() else {
        return pg_unavailable_response();
    };
    rebind_slot_with_pg(&agent_id, slot_index, &body, &pg_pool).await
}

/// PATCH /api/auto-queue/entries/{id}/skip
async fn skip_entry_with_pg(
    id: &str,
    pool: &sqlx::PgPool,
) -> (StatusCode, Json<serde_json::Value>) {
    match crate::db::auto_queue::update_entry_status_on_pg(
        pool,
        id,
        crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
        "manual_skip",
        &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
    )
    .await
    {
        Ok(result) if result.changed => {}
        Ok(_) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "entry not found or not pending"})),
            );
        }
        Err(error) if error.contains("not found") => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "entry not found"})),
            );
        }
        Err(error) if error.contains("invalid auto-queue entry transition") => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "only pending entries can be skipped"})),
            );
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            );
        }
    }

    (StatusCode::OK, Json(json!({ "ok": true })))
}

pub async fn skip_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pg_pool) = state.pg_pool.clone() else {
        return pg_unavailable_response();
    };
    skip_entry_with_pg(&id, &pg_pool).await
}

