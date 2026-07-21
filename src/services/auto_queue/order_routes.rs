use super::*;
use axum::extract::Extension;

use crate::api_caller_observability::{
    RequestPrincipal, log_identity_consumption, manager_channel_check_relied_on_claimed_header,
};

// ── Authenticated order submission callback ─────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OrderBody {
    /// Ordered list of GitHub issue numbers (or card IDs)
    pub order: Vec<serde_json::Value>,
    pub rationale: Option<String>,
    /// Alias for rationale (compatibility)
    pub reasoning: Option<String>,
}

/// POST /api/queue/runs/:id/order
/// Authenticated callback: provides the ordered card list for a pending run.
pub(super) async fn resolve_submit_order_card_with_pg(
    pool: &sqlx::PgPool,
    run_repo: Option<&str>,
    item: &serde_json::Value,
) -> Result<Option<ResolvedDispatchCard>, String> {
    let row = if let Some(issue_number) = item.as_i64() {
        sqlx::query(
            "SELECT id,
                    repo_id,
                    status,
                    assigned_agent_id,
                    github_issue_number::BIGINT AS github_issue_number
             FROM kanban_cards
             WHERE github_issue_number = $1
               AND ($2::TEXT IS NULL OR repo_id = $2)
             ORDER BY id ASC
             LIMIT 1",
        )
        .bind(issue_number)
        .bind(run_repo)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load kanban card for issue #{issue_number}: {error}"))?
    } else if let Some(card_id) = item.as_str() {
        sqlx::query(
            "SELECT id,
                    repo_id,
                    status,
                    assigned_agent_id,
                    github_issue_number::BIGINT AS github_issue_number
             FROM kanban_cards
             WHERE id = $1
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load kanban card {card_id}: {error}"))?
    } else {
        None
    };

    let Some(row) = row else {
        return Ok(None);
    };

    Ok(Some(ResolvedDispatchCard {
        issue_number: row
            .try_get("github_issue_number")
            .map_err(|error| format!("decode github_issue_number: {error}"))?,
        card_id: row
            .try_get("id")
            .map_err(|error| format!("decode card id: {error}"))?,
        repo_id: row
            .try_get("repo_id")
            .map_err(|error| format!("decode repo_id: {error}"))?,
        status: row
            .try_get("status")
            .map_err(|error| format!("decode status: {error}"))?,
        assigned_agent_id: row
            .try_get("assigned_agent_id")
            .map_err(|error| format!("decode assigned_agent_id: {error}"))?,
    }))
}

pub(super) async fn submit_order_with_pg(
    state: &AppState,
    run_id: &str,
    headers: &HeaderMap,
    principal: Option<&RequestPrincipal>,
    body: &OrderBody,
    pool: &sqlx::PgPool,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let caller_agent_id =
        crate::services::kanban::resolve_requesting_agent_id_with_pg(pool, headers).await;
    let config = crate::config::load_graceful();
    log_identity_consumption(
        "POST /api/queue/runs/{id}/order",
        principal,
        caller_agent_id.as_deref(),
        manager_channel_check_relied_on_claimed_header(
            headers,
            config.kanban.manager_channel_id.as_deref(),
        ),
    );
    let run_row = match sqlx::query(
        "SELECT status, repo
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            return Err(auto_queue_json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("load auto-queue run '{run_id}': {error}")})),
            ));
        }
    };
    let Some(run_row) = run_row else {
        return Err(auto_queue_json_error(
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "run not found or not pending"})),
        ));
    };
    let run_status: String = match run_row.try_get("status") {
        Ok(value) => value,
        Err(error) => {
            return Err(auto_queue_json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue run status: {error}")})),
            ));
        }
    };
    if run_status != "pending" {
        return Err(auto_queue_json_error(
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "run not found or not pending"})),
        ));
    }
    let run_repo: Option<String> = match run_row.try_get("repo") {
        Ok(value) => value,
        Err(error) => {
            return Err(auto_queue_json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("decode auto-queue run repo: {error}")})),
            ));
        }
    };
    let run_log_ctx = AutoQueueLogContext::new().run(run_id);

    let mut created = 0;
    for (rank, item) in body.order.iter().enumerate() {
        let card = match resolve_submit_order_card_with_pg(pool, run_repo.as_deref(), item).await {
            Ok(Some(card)) => card,
            Ok(None) => continue,
            Err(error) => {
                return Err(auto_queue_json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                ));
            }
        };

        let dispatchable_check = crate::pipeline::try_get()
            .map(|pipeline| {
                pipeline
                    .dispatchable_states()
                    .iter()
                    .any(|state| *state == card.status)
            })
            .unwrap_or(card.status == "ready");
        if !dispatchable_check {
            crate::auto_queue_log!(
                info,
                "submit_order_card_not_dispatchable",
                run_log_ctx.clone().card(&card.card_id),
                "[auto-queue] Skipping card {} (status={}, not dispatchable)",
                card.card_id,
                card.status
            );
            continue;
        }

        let entry_id = uuid::Uuid::new_v4().to_string();
        if sqlx::query(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&entry_id)
        .bind(run_id)
        .bind(&card.card_id)
        .bind(card.assigned_agent_id.as_deref().unwrap_or(""))
        .bind(rank as i64)
        .execute(pool)
        .await
        .is_ok()
        {
            created += 1;
        }
    }

    let rationale = body
        .rationale
        .clone()
        .or(body.reasoning.clone())
        .unwrap_or_else(|| {
            caller_agent_id
                .as_deref()
                .map(|agent_id| format!("{agent_id} order submitted"))
                .unwrap_or_else(|| "API order submitted".to_string())
        });
    if created > 0 {
        if let Err(error) = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'active',
                 ai_rationale = $1
             WHERE id = $2",
        )
        .bind(&rationale)
        .bind(run_id)
        .execute(pool)
        .await
        {
            return Err(auto_queue_json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("activate auto-queue run '{run_id}': {error}")})),
            ));
        }
    } else {
        crate::auto_queue_log!(
            warn,
            "submit_order_no_ready_cards",
            run_log_ctx.clone(),
            "[auto-queue] submit_order: no ready cards enqueued, run {run_id} stays pending"
        );
        if let Err(error) = sqlx::query(
            "UPDATE auto_queue_runs
             SET status = 'completed',
                 ai_rationale = $1
             WHERE id = $2",
        )
        .bind(format!("{rationale} (no ready cards — auto-completed)"))
        .bind(run_id)
        .execute(pool)
        .await
        {
            return Err(auto_queue_json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("complete auto-queue run '{run_id}': {error}")})),
            ));
        }
    }

    let _ = state;

    Ok((
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "created": created,
            "run_id": run_id,
            "message": "Queue active. Call POST /api/queue/dispatch-next to start dispatching.",
        })),
    ))
}

pub async fn submit_order(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    principal: Option<Extension<RequestPrincipal>>,
    Json(body): Json<OrderBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if let Err(response) =
        crate::services::kanban::require_explicit_bearer_token(&headers, "submit_order")
    {
        return Err(auto_queue_tuple_error(response));
    }
    let Some(pg_pool) = state.pg_pool.clone() else {
        return Err(auto_queue_tuple_error(pg_unavailable_response()));
    };
    submit_order_with_pg(
        &state,
        &run_id,
        &headers,
        principal.as_ref().map(|Extension(principal)| principal),
        &body,
        &pg_pool,
    )
    .await
}
