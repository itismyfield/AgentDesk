use super::*;

/// GET /api/queue/status
pub async fn status(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    let input = crate::services::auto_queue::StatusInput {
        repo: query.repo,
        agent_id: query.agent_id,
        guild_id: state.config.discord.guild_id.clone(),
    };

    let result = state.auto_queue_service().status_with_pg(pool, input).await;

    match result {
        Ok(response) => (StatusCode::OK, Json(json!(response))),
        Err(error) => error.into_json_response(),
    }
}

/// GET /api/queue/history
pub async fn history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let limit = query.limit.unwrap_or(8).clamp(1, 20);
    let filter = crate::db::auto_queue::StatusFilter {
        repo: query.repo,
        agent_id: query.agent_id,
    };
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable_response();
    };
    let records = match crate::db::auto_queue::list_run_history_pg(pool, &filter, limit).await {
        Ok(records) => records,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("list run history: {error}")})),
            );
        }
    };

    let now_ms = chrono::Utc::now().timestamp_millis();
    let runs: Vec<AutoQueueHistoryRun> = records
        .into_iter()
        .map(|record| {
            let entry_count = record.entry_count.max(0);
            let completed_count = record.done_count.max(0);
            let unresolved_count = (entry_count - completed_count).max(0) as f64;
            let total_entries = entry_count.max(1) as f64;
            let success_rate = if entry_count > 0 {
                completed_count as f64 / total_entries
            } else {
                0.0
            };
            let failure_rate = if entry_count > 0 {
                unresolved_count / total_entries
            } else {
                0.0
            };
            let duration_ms = record
                .completed_at
                .unwrap_or(now_ms)
                .saturating_sub(record.created_at);
            let timeout_ms = record.timeout_minutes.max(0).saturating_mul(60_000);
            let timeout_exceeded = timeout_ms > 0 && duration_ms > timeout_ms;
            let timeout_overrun_ms = if timeout_exceeded {
                duration_ms.saturating_sub(timeout_ms)
            } else {
                0
            };

            AutoQueueHistoryRun {
                id: record.id,
                repo: record.repo,
                agent_id: record.agent_id,
                status: record.status,
                timeout_minutes: record.timeout_minutes,
                timeout_exceeded,
                timeout_overrun_ms,
                created_at: record.created_at,
                completed_at: record.completed_at,
                duration_ms,
                entry_count,
                done_count: record.done_count,
                skipped_count: record.skipped_count,
                pending_count: record.pending_count,
                dispatched_count: record.dispatched_count,
                success_rate,
                failure_rate,
            }
        })
        .collect();

    let total_runs = runs.len();
    let completed_runs = runs.iter().filter(|run| run.status == "completed").count();
    let success_rate = if total_runs > 0 {
        runs.iter().map(|run| run.success_rate).sum::<f64>() / total_runs as f64
    } else {
        0.0
    };
    let failure_rate = if total_runs > 0 {
        runs.iter().map(|run| run.failure_rate).sum::<f64>() / total_runs as f64
    } else {
        0.0
    };

    (
        StatusCode::OK,
        Json(json!({
            "summary": AutoQueueHistorySummary {
                total_runs,
                completed_runs,
                success_rate,
                failure_rate,
            },
            "runs": runs,
        })),
    )
}

/// PATCH /api/queue/entries/{id}
pub(super) async fn update_entry_with_pg(
    state: &AppState,
    id: &str,
    body: &UpdateEntryBody,
    requested_status: Option<&str>,
    pool: &sqlx::PgPool,
) -> (StatusCode, Json<serde_json::Value>) {
    let entry_row = match sqlx::query(
        "SELECT run_id, status
         FROM auto_queue_entries
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("load auto-queue entry {id}: {error}")})),
            );
        }
    };
    let Some(entry_row) = entry_row else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "entry not found"})),
        );
    };

    let run_id: String = match entry_row.try_get("run_id") {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue entry run_id: {error}")})),
            );
        }
    };
    let status: String = match entry_row.try_get("status") {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue entry status: {error}")})),
            );
        }
    };

    let mut effective_status = status.clone();
    if let Some(new_status) = requested_status {
        let update_result = if new_status == crate::db::auto_queue::ENTRY_STATUS_DONE {
            crate::db::auto_queue::reconcile_failed_entry_done_on_pg(
                pool,
                id,
                "manual_terminal_reconcile",
            )
            .await
        } else {
            crate::db::auto_queue::update_entry_status_on_pg(
                pool,
                id,
                new_status,
                "manual_update",
                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
            )
            .await
        };
        match update_result {
            Ok(result) => effective_status = result.to_status,
            Err(error) if error.contains("not found") => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "entry not found"})),
                );
            }
            Err(error) if error.contains("invalid auto-queue entry transition") => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": format!(
                            "entry status transition not allowed: {} -> {}",
                            status, new_status
                        ),
                    })),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
    }

    if body.thread_group.is_some() || body.priority_rank.is_some() || body.batch_phase.is_some() {
        if effective_status != crate::db::auto_queue::ENTRY_STATUS_PENDING {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "only pending entries can be reprioritized"})),
            );
        }

        let mut tx = match pool.begin().await {
            Ok(tx) => tx,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("open update_entry transaction: {error}")})),
                );
            }
        };
        let changed = match sqlx::query(
            "UPDATE auto_queue_entries
             SET thread_group = COALESCE($1, thread_group),
                 priority_rank = COALESCE($2, priority_rank),
                 batch_phase = COALESCE($3, batch_phase)
             WHERE id = $4
               AND status = 'pending'",
        )
        .bind(body.thread_group)
        .bind(body.priority_rank)
        .bind(body.batch_phase)
        .bind(id)
        .execute(&mut *tx)
        .await
        {
            Ok(result) => result.rows_affected(),
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("update auto-queue entry {id}: {error}")})),
                );
            }
        };
        if changed == 0 {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "entry not found or not pending"})),
            );
        }

        if body.thread_group.is_some() {
            if let Err(error) = sync_run_group_metadata_with_pg_tx(&mut tx, &run_id).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }

        if let Err(error) = tx.commit().await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("commit update_entry transaction: {error}")})),
            );
        }
    }

    let entry = state
        .auto_queue_service()
        .entry_json_with_pg(pool, id, None)
        .await
        .unwrap_or(serde_json::Value::Null);

    (StatusCode::OK, Json(json!({ "ok": true, "entry": entry })))
}

pub async fn update_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateEntryBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.thread_group.is_none()
        && body.priority_rank.is_none()
        && body.batch_phase.is_none()
        && body.status.is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }
    if let Some(thread_group) = body.thread_group {
        if thread_group < 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "thread_group must be >= 0"})),
            );
        }
    }
    if let Some(priority_rank) = body.priority_rank {
        if priority_rank < 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "priority_rank must be >= 0"})),
            );
        }
    }
    if let Some(batch_phase) = body.batch_phase {
        if batch_phase < 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "batch_phase must be >= 0"})),
            );
        }
    }
    let requested_status = match body.status.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(crate::db::auto_queue::ENTRY_STATUS_PENDING) => {
            Some(crate::db::auto_queue::ENTRY_STATUS_PENDING)
        }
        Some(crate::db::auto_queue::ENTRY_STATUS_SKIPPED) => {
            Some(crate::db::auto_queue::ENTRY_STATUS_SKIPPED)
        }
        Some(crate::db::auto_queue::ENTRY_STATUS_DONE) => {
            Some(crate::db::auto_queue::ENTRY_STATUS_DONE)
        }
        Some(crate::db::auto_queue::ENTRY_STATUS_DISPATCHED) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "manual entry status updates only support pending, skipped, or terminal done reconciliation"
                })),
            );
        }
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("unsupported entry status '{other}'")})),
            );
        }
    };

    let Some(pg_pool) = state.pg_pool.clone() else {
        return pg_unavailable_response();
    };
    update_entry_with_pg(&state, &id, &body, requested_status, &pg_pool).await
}

/// POST /api/queue/runs/{id}/entries
pub(super) async fn add_run_entry_with_pg(
    state: &AppState,
    run_id: &str,
    body: &AddRunEntryBody,
    batch_phase: i64,
    pool: &sqlx::PgPool,
) -> (StatusCode, Json<serde_json::Value>) {
    let run_row = match sqlx::query(
        "SELECT status, repo, agent_id
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("load auto-queue run '{run_id}': {error}")})),
            );
        }
    };
    let Some(run_row) = run_row else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("auto-queue run '{run_id}' not found")})),
        );
    };

    let run_status: String = match run_row.try_get("status") {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue run status: {error}")})),
            );
        }
    };
    let run_repo: Option<String> = match run_row.try_get("repo") {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue run repo: {error}")})),
            );
        }
    };
    let run_agent_id: Option<String> = match run_row.try_get("agent_id") {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue run agent: {error}")})),
            );
        }
    };
    if run_status != "active" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("auto-queue run '{run_id}' is not active (status={run_status})"),
                "run_id": run_id,
                "status": run_status,
            })),
        );
    }

    let issue_numbers = [body.issue_number];
    let cards_by_issue =
        match resolve_dispatch_cards_with_pg(pool, run_repo.as_deref(), &issue_numbers).await {
            Ok(cards) => cards,
            Err(err) => {
                let status = if err.contains("not found") {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::BAD_REQUEST
                };
                return (status, Json(json!({"error": err})));
            }
        };
    let Some(card) = cards_by_issue.get(&body.issue_number) else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({"error": format!("kanban card not found for issue #{}", body.issue_number)}),
            ),
        );
    };
    if card.status != "ready" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "issue #{} must be in ready status to be added to an active run (current={})",
                    body.issue_number,
                    card.status
                )
            })),
        );
    }

    let run_agent = run_agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let card_agent = card
        .assigned_agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (run_agent, card_agent) {
        (_, None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("issue #{} has no assigned agent", body.issue_number)
                })),
            );
        }
        (Some(run_agent), Some(card_agent)) if run_agent != card_agent => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!(
                        "issue #{} is assigned to {}, not the active run agent {}",
                        body.issue_number,
                        card_agent,
                        run_agent
                    )
                })),
            );
        }
        _ => {}
    }

    let inserted = match enqueue_entries_into_existing_run_with_pg(
        pool,
        run_id,
        &[GenerateEntryBody {
            issue_number: body.issue_number,
            batch_phase: Some(batch_phase),
            thread_group: body.thread_group,
        }],
        &cards_by_issue,
    )
    .await
    {
        Ok(entries) => entries,
        Err(err) => {
            let status = if err.contains("already queued") || err.contains("active dispatch") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            return (status, Json(json!({"error": err})));
        }
    };
    let Some(inserted_entry) = inserted.into_iter().next() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to create auto-queue entry"})),
        );
    };
    let entry = state
        .auto_queue_service()
        .entry_json_with_pg(pool, &inserted_entry.entry_id, None)
        .await
        .unwrap_or(serde_json::Value::Null);

    (
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "run_id": run_id,
            "thread_group": inserted_entry.thread_group,
            "priority_rank": inserted_entry.priority_rank,
            "entry": entry,
        })),
    )
}

pub async fn add_run_entry(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<AddRunEntryBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.issue_number <= 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "issue_number must be > 0"})),
        );
    }
    if let Some(thread_group) = body.thread_group {
        if thread_group < 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "thread_group must be >= 0"})),
            );
        }
    }
    let batch_phase = body.batch_phase.unwrap_or(0);
    if batch_phase < 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "batch_phase must be >= 0"})),
        );
    }
    let Some(pg_pool) = state.pg_pool.clone() else {
        return pg_unavailable_response();
    };
    add_run_entry_with_pg(&state, &run_id, &body, batch_phase, &pg_pool).await
}

/// POST /api/queue/runs/{id}/restore
pub(super) async fn restore_run_with_pg(
    state: &AppState,
    run_id: &str,
    pool: &sqlx::PgPool,
) -> (StatusCode, Json<serde_json::Value>) {
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
        Some("cancelled") | Some(RUN_STATUS_RESTORING) => {}
        Some("active") => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("auto-queue run '{run_id}' is already active")})),
            );
        }
        Some(status) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!(
                        "only cancelled or restoring runs can be restored (status={status})"
                    ),
                    "run_id": run_id,
                    "status": status,
                })),
            );
        }
    }

    let deps = AutoQueueActivateDeps::from_state(state);
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut counts = RestoreRunCounts::default();
    let mut dispatch_candidates = Vec::new();

    match apply_restore_state_changes_pg(pool, run_id, run_status.as_deref()).await {
        Ok((applied_counts, candidates)) => {
            counts = applied_counts;
            dispatch_candidates = candidates;
        }
        Err(error) => errors.push(error),
    }

    if errors.is_empty() {
        for candidate in &dispatch_candidates {
            match attempt_restore_dispatch(&deps, run_id, candidate) {
                Ok(result) => {
                    if result.dispatched {
                        counts.restored_pending = counts.restored_pending.saturating_sub(1);
                        counts.restored_dispatched += 1;
                    }
                    if result.created_dispatch {
                        counts.created_dispatches += 1;
                    }
                    if result.rebound_slot {
                        counts.rebound_slots += 1;
                    }
                    if result.unbound_dispatch {
                        counts.unbound_dispatches += 1;
                    }
                }
                Err(error) => warnings.push(error),
            }
        }

        if let Err(error) = finalize_restore_run_pg(pool, run_id).await {
            errors.push(error);
        }
    }

    let final_run_status = sqlx::query_scalar::<_, String>(
        "SELECT status
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| "unknown".to_string());

    let mut payload = json!({
        "ok": errors.is_empty(),
        "run_id": run_id,
        "run_status": final_run_status,
        "restored_pending": counts.restored_pending,
        "restored_done": counts.restored_done,
        "restored_dispatched": counts.restored_dispatched,
        "rebound_slots": counts.rebound_slots,
        "created_dispatches": counts.created_dispatches,
        "unbound_dispatches": counts.unbound_dispatches,
    });
    if !errors.is_empty() {
        payload["errors"] = json!(errors);
    }
    if counts.unbound_dispatches > 0 {
        warnings.push(format!(
            "{} restored dispatch(es) still need slot rebind",
            counts.unbound_dispatches
        ));
    }
    if !warnings.is_empty() {
        payload["warning"] = json!(warnings.join("; "));
    }

    (StatusCode::OK, Json(payload))
}

pub async fn restore_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pg_pool) = state.pg_pool.clone() else {
        return pg_unavailable_response();
    };
    restore_run_with_pg(&state, &run_id, &pg_pool).await
}
