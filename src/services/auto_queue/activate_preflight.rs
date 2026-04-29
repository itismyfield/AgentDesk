enum ActivatePgPreflight {
    Return((StatusCode, Json<serde_json::Value>)),
    Continue(ActivateBody),
}

async fn activate_preflight_with_pg(
    pool: &sqlx::PgPool,
    mut body: ActivateBody,
) -> ActivatePgPreflight {
    let active_only = body.active_only.unwrap_or(false);
    let selected_run = if let Some(run_id) = body
        .run_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        match sqlx::query(
            "SELECT id, status
             FROM auto_queue_runs
             WHERE id = $1",
        )
        .bind(run_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(row)) => {
                let id = match row.try_get::<String, _>("id") {
                    Ok(value) => value,
                    Err(error) => {
                        return ActivatePgPreflight::Return((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(
                                json!({"error": format!("decode postgres auto-queue run {run_id}: {error}")}),
                            ),
                        ));
                    }
                };
                let status = match row.try_get::<String, _>("status") {
                    Ok(value) => value,
                    Err(error) => {
                        return ActivatePgPreflight::Return((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(
                                json!({"error": format!("decode postgres auto-queue run status {run_id}: {error}")}),
                            ),
                        ));
                    }
                };
                Some((id, status))
            }
            Ok(None) => None,
            Err(error) => {
                return ActivatePgPreflight::Return((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres auto-queue run {run_id}: {error}")}),
                    ),
                ));
            }
        }
    } else {
        let repo = body
            .repo
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let agent_id = body
            .agent_id
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let status_clause = if active_only {
            "status = 'active'"
        } else {
            "status IN ('active', 'generated', 'pending')"
        };
        let query = format!(
            "SELECT id, status
             FROM auto_queue_runs
             WHERE ($1::TEXT IS NULL OR repo = $1 OR repo IS NULL OR repo = '')
               AND ($2::TEXT IS NULL OR agent_id = $2 OR agent_id IS NULL OR agent_id = '')
               AND {status_clause}
             ORDER BY created_at DESC
             LIMIT 1"
        );
        match sqlx::query(&query)
            .bind(repo)
            .bind(agent_id)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(row)) => {
                let id = match row.try_get::<String, _>("id") {
                    Ok(value) => value,
                    Err(error) => {
                        return ActivatePgPreflight::Return((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(
                                json!({"error": format!("decode postgres auto-queue selected run id: {error}")}),
                            ),
                        ));
                    }
                };
                let status = match row.try_get::<String, _>("status") {
                    Ok(value) => value,
                    Err(error) => {
                        return ActivatePgPreflight::Return((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(
                                json!({"error": format!("decode postgres auto-queue selected run status: {error}")}),
                            ),
                        ));
                    }
                };
                Some((id, status))
            }
            Ok(None) => None,
            Err(error) => {
                return ActivatePgPreflight::Return((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        json!({"error": format!("load postgres auto-queue selected run: {error}")}),
                    ),
                ));
            }
        }
    };

    let Some((run_id, status)) = selected_run else {
        return ActivatePgPreflight::Return((
            StatusCode::OK,
            Json(json!({ "dispatched": [], "count": 0, "message": "No active run" })),
        ));
    };

    let blocking_phase_gate = match sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1
             FROM auto_queue_phase_gates
             WHERE run_id = $1
               AND status IN ('pending', 'failed')
         )",
    )
    .bind(&run_id)
    .fetch_one(pool)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            return ActivatePgPreflight::Return((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": format!("load postgres auto-queue phase gates for {run_id}: {error}")}),
                ),
            ));
        }
    };

    match status.as_str() {
        "paused" => {
            let message = if blocking_phase_gate {
                "Run is waiting on phase gate"
            } else {
                "Run is paused"
            };
            return ActivatePgPreflight::Return((
                StatusCode::OK,
                Json(json!({ "dispatched": [], "count": 0, "message": message })),
            ));
        }
        RUN_STATUS_RESTORING => {
            return ActivatePgPreflight::Return((
                StatusCode::OK,
                Json(json!({ "dispatched": [], "count": 0, "message": "Run is restoring" })),
            ));
        }
        _ if active_only && status != "active" => {
            return ActivatePgPreflight::Return((
                StatusCode::OK,
                Json(json!({ "dispatched": [], "count": 0, "message": "No active run" })),
            ));
        }
        _ if blocking_phase_gate => {
            return ActivatePgPreflight::Return((
                StatusCode::OK,
                Json(
                    json!({ "dispatched": [], "count": 0, "message": "Run is waiting on phase gate" }),
                ),
            ));
        }
        _ => {}
    }

    if body.run_id.is_none() {
        body.run_id = Some(run_id);
    }

    ActivatePgPreflight::Continue(body)
}

