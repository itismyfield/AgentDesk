use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;

use crate::server::routes::AppState;

const VALID_DISPATCH_STATUSES: &[&str] =
    &["pending", "dispatched", "completed", "cancelled", "failed"];

// ── Query / Body types ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListDispatchesQuery {
    pub status: Option<String>,
    pub kanban_card_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateDispatchBody {
    pub kanban_card_id: String,
    pub to_agent_id: String,
    pub dispatch_type: Option<String>,
    pub title: String,
    pub context: Option<serde_json::Value>,
    pub skip_outbox: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpdateDispatchBody {
    pub status: Option<String>,
    pub result: Option<serde_json::Value>,
}

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/dispatches
pub async fn list_dispatches(
    State(state): State<AppState>,
    Query(params): Query<ListDispatchesQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    match list_dispatches_pg(
        pool,
        params.status.as_deref(),
        params.kanban_card_id.as_deref(),
    )
    .await
    {
        Ok(dispatches) => (StatusCode::OK, Json(json!({"dispatches": dispatches}))),
        Err(error) => internal_error(error),
    }
}

/// GET /api/dispatches/:id
pub async fn get_dispatch(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    match crate::dispatch::query_dispatch_row_pg(pool, &id).await {
        Ok(dispatch) => (StatusCode::OK, Json(json!({"dispatch": dispatch}))),
        Err(error) if error.to_string().contains("Query returned no rows") => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "dispatch not found"})),
        ),
        Err(error) => internal_error(format!("{error}")),
    }
}

/// POST /api/dispatches
pub async fn create_dispatch(
    State(state): State<AppState>,
    Json(body): Json<CreateDispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    let dispatch_type = body
        .dispatch_type
        .unwrap_or_else(|| "implementation".to_string());
    let to_agent_id = resolve_dispatch_target_agent_id_pg(pool, &body.to_agent_id)
        .await
        .unwrap_or(body.to_agent_id);
    let context = body.context.unwrap_or_else(|| json!({}));
    let options = crate::dispatch::DispatchCreateOptions {
        skip_outbox: body.skip_outbox.unwrap_or(false),
        sidecar_dispatch: context
            .get("sidecar_dispatch")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
            || context
                .get("phase_gate")
                .and_then(|value| value.as_object())
                .is_some(),
    };

    let result = crate::dispatch::create_dispatch_core_with_options(
        pool,
        &body.kanban_card_id,
        &to_agent_id,
        &dispatch_type,
        &body.title,
        &context,
        options,
    )
    .await;

    match result {
        Ok((dispatch_id, _, reused)) => {
            match crate::dispatch::query_dispatch_row_pg(pool, &dispatch_id).await {
                Ok(dispatch) => (
                    if reused {
                        StatusCode::OK
                    } else {
                        StatusCode::CREATED
                    },
                    Json(json!({"dispatch": dispatch})),
                ),
                Err(error) => internal_error(format!("{error}")),
            }
        }
        Err(error) => dispatch_create_error(format!("{error}")),
    }
}

/// PATCH /api/dispatches/:id
pub async fn update_dispatch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    if body.status.as_deref() == Some("completed") {
        if let Ok(dispatch) = crate::dispatch::query_dispatch_row_pg(pool, &id).await {
            let is_review = dispatch
                .get("dispatch_type")
                .and_then(|value| value.as_str())
                == Some("review");
            let has_verdict = body
                .result
                .as_ref()
                .and_then(|result| result.get("verdict").or_else(|| result.get("decision")))
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.is_empty());
            if is_review && !has_verdict {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(
                        json!({"error": "review dispatch completion requires explicit verdict — use POST /api/review-verdict"}),
                    ),
                );
            }
        }

        return match crate::dispatch::finalize_dispatch_with_backends(
            None,
            &state.engine,
            &id,
            "api",
            body.result.as_ref(),
        ) {
            Ok(dispatch) => (StatusCode::OK, Json(json!({"dispatch": dispatch}))),
            Err(error) => dispatch_update_error(&id, format!("{error}")),
        };
    }

    if let Some(status) = body.status.as_deref()
        && !VALID_DISPATCH_STATUSES.contains(&status)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": format!("invalid dispatch status '{}' — allowed values: {}", status, VALID_DISPATCH_STATUSES.join(", "))}),
            ),
        );
    }

    if let Some(status) = body.status {
        let changed = crate::dispatch::set_dispatch_status_with_backends(
            None,
            Some(pool),
            &id,
            &status,
            body.result.as_ref(),
            "api_update_dispatch",
            None,
            false,
        );
        match changed {
            Ok(0) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "dispatch not found"})),
                );
            }
            Ok(_) => {}
            Err(error) => return internal_error(format!("{error}")),
        }
    } else if let Some(result) = body.result {
        match update_dispatch_result_pg(pool, &id, &result).await {
            Ok(0) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "dispatch not found"})),
                );
            }
            Ok(_) => {}
            Err(error) => return internal_error(error),
        }
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    match crate::dispatch::query_dispatch_row_pg(pool, &id).await {
        Ok(dispatch) => (StatusCode::OK, Json(json!({"dispatch": dispatch}))),
        Err(error) => internal_error(format!("{error}")),
    }
}

fn pg_unavailable() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "postgres pool unavailable"})),
    )
}

fn internal_error(error: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": error.into()})),
    )
}

fn dispatch_create_error(message: String) -> (StatusCode, Json<serde_json::Value>) {
    if message.contains("not found") {
        (StatusCode::NOT_FOUND, Json(json!({"error": message})))
    } else if message.starts_with("Cannot create ") || message.contains("already exists") {
        (StatusCode::CONFLICT, Json(json!({"error": message})))
    } else {
        internal_error(message)
    }
}

fn dispatch_update_error(
    dispatch_id: &str,
    message: String,
) -> (StatusCode, Json<serde_json::Value>) {
    if message.contains("not found") {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": message, "dispatch_id": dispatch_id})),
        )
    } else if message.contains("no agent execution evidence") {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": message, "dispatch_id": dispatch_id})),
        )
    } else {
        internal_error(message)
    }
}

async fn list_dispatches_pg(
    pool: &sqlx::PgPool,
    status: Option<&str>,
    kanban_card_id: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    let rows = sqlx::query(
        "SELECT
            id,
            kanban_card_id,
            from_agent_id,
            to_agent_id,
            dispatch_type,
            status,
            title,
            context,
            result,
            parent_dispatch_id,
            COALESCE(chain_depth, 0)::BIGINT AS chain_depth,
            created_at::text AS created_at,
            updated_at::text AS updated_at,
            completed_at::text AS completed_at
         FROM task_dispatches
         WHERE ($1::text IS NULL OR status = $1)
           AND ($2::text IS NULL OR kanban_card_id = $2)
         ORDER BY created_at DESC",
    )
    .bind(status)
    .bind(kanban_card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("list postgres dispatches: {error}"))?;

    rows.into_iter()
        .map(|row| {
            let status = row
                .try_get::<String, _>("status")
                .map_err(|error| format!("decode postgres dispatch status: {error}"))?;
            let dispatch_type = row
                .try_get::<Option<String>, _>("dispatch_type")
                .map_err(|error| format!("decode postgres dispatch type: {error}"))?;
            let context_raw = row
                .try_get::<Option<String>, _>("context")
                .map_err(|error| format!("decode postgres dispatch context: {error}"))?;
            let result_raw = row
                .try_get::<Option<String>, _>("result")
                .map_err(|error| format!("decode postgres dispatch result: {error}"))?;
            let context = context_raw
                .as_deref()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
            let result = result_raw
                .as_deref()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
            let result_summary = crate::dispatch::summarize_dispatch_result(
                dispatch_type.as_deref(),
                Some(status.as_str()),
                result.as_ref(),
                context.as_ref(),
            );
            let created_at = row
                .try_get::<String, _>("created_at")
                .map_err(|error| format!("decode postgres dispatch created_at: {error}"))?;
            let updated_at = row
                .try_get::<String, _>("updated_at")
                .map_err(|error| format!("decode postgres dispatch updated_at: {error}"))?;
            let completed_at = row
                .try_get::<Option<String>, _>("completed_at")
                .map_err(|error| format!("decode postgres dispatch completed_at: {error}"))?
                .or_else(|| (status == "completed").then(|| updated_at.clone()));

            Ok(json!({
                "id": row.try_get::<String, _>("id").map_err(|error| format!("decode postgres dispatch id: {error}"))?,
                "kanban_card_id": row.try_get::<Option<String>, _>("kanban_card_id").map_err(|error| format!("decode postgres dispatch kanban_card_id: {error}"))?,
                "from_agent_id": row.try_get::<Option<String>, _>("from_agent_id").map_err(|error| format!("decode postgres dispatch from_agent_id: {error}"))?,
                "to_agent_id": row.try_get::<Option<String>, _>("to_agent_id").map_err(|error| format!("decode postgres dispatch to_agent_id: {error}"))?,
                "dispatch_type": dispatch_type,
                "status": status,
                "title": row.try_get::<Option<String>, _>("title").map_err(|error| format!("decode postgres dispatch title: {error}"))?,
                "context": context,
                "result": result,
                "context_file": serde_json::Value::Null,
                "result_file": serde_json::Value::Null,
                "result_summary": result_summary,
                "parent_dispatch_id": row.try_get::<Option<String>, _>("parent_dispatch_id").map_err(|error| format!("decode postgres dispatch parent_dispatch_id: {error}"))?,
                "chain_depth": row.try_get::<i64, _>("chain_depth").map_err(|error| format!("decode postgres dispatch chain_depth: {error}"))?,
                "created_at": created_at.clone(),
                "dispatched_at": Some(created_at),
                "updated_at": updated_at,
                "completed_at": completed_at,
            }))
        })
        .collect()
}

async fn resolve_dispatch_target_agent_id_pg(
    pool: &sqlx::PgPool,
    raw_target: &str,
) -> Option<String> {
    let exact_match = sqlx::query("SELECT id FROM agents WHERE id = $1 LIMIT 1")
        .bind(raw_target)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .and_then(|row| row.try_get::<String, _>("id").ok());
    if exact_match.is_some() {
        return exact_match;
    }

    sqlx::query(
        "SELECT id FROM agents
         WHERE discord_channel_id = $1
            OR discord_channel_alt = $1
            OR discord_channel_cc = $1
            OR discord_channel_cdx = $1
         LIMIT 1",
    )
    .bind(raw_target)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .and_then(|row| row.try_get::<String, _>("id").ok())
}

async fn update_dispatch_result_pg(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<usize, String> {
    let result_json = serde_json::to_string(result)
        .map_err(|error| format!("serialize dispatch result {dispatch_id}: {error}"))?;
    let updated = sqlx::query(
        "UPDATE task_dispatches
         SET result = $2,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .bind(result_json)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres dispatch result {dispatch_id}: {error}"))?;
    Ok(updated.rows_affected() as usize)
}
