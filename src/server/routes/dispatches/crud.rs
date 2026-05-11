use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

use crate::server::dto::dispatches::{DispatchListItem, DispatchRouteResponse};
use crate::server::routes::AppState;

const VALID_DISPATCH_STATUSES: &[&str] =
    &["pending", "dispatched", "completed", "cancelled", "failed"];

// ── Query / Body types ─────────────────────────────────────────

// #2050 P2 finding 4 — accept previously-silently-dropped client filters
// (`from_agent_id` / `to_agent_id` / `limit`) so dashboard queries actually
// scope the result instead of transferring the whole table.
#[derive(Debug, Deserialize)]
pub struct ListDispatchesQuery {
    pub status: Option<String>,
    pub kanban_card_id: Option<String>,
    pub from_agent_id: Option<String>,
    pub to_agent_id: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CreateDispatchBody {
    pub kanban_card_id: String,
    pub to_agent_id: String,
    pub dispatch_type: Option<String>,
    pub title: String,
    pub context: Option<serde_json::Value>,
    pub required_capabilities: Option<serde_json::Value>,
    pub skip_outbox: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpdateDispatchBody {
    pub status: Option<String>,
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_from: Option<Vec<String>>,
}

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/dispatches
pub async fn list_dispatches(
    State(state): State<AppState>,
    Query(params): Query<ListDispatchesQuery>,
) -> (StatusCode, Json<DispatchRouteResponse>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    match list_dispatches_pg(
        pool,
        params.status.as_deref(),
        params.kanban_card_id.as_deref(),
        params.from_agent_id.as_deref(),
        params.to_agent_id.as_deref(),
        params.limit,
    )
    .await
    {
        Ok(dispatches) => (
            StatusCode::OK,
            Json(DispatchRouteResponse::list(dispatches)),
        ),
        Err(error) => internal_error(error),
    }
}

/// GET /api/dispatches/:id
pub async fn get_dispatch(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<DispatchRouteResponse>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    match crate::dispatch::query_dispatch_row_pg(pool, &id).await {
        Ok(dispatch) => (
            StatusCode::OK,
            Json(DispatchRouteResponse::dispatch(dispatch)),
        ),
        Err(error) if error.to_string().contains("Query returned no rows") => (
            StatusCode::NOT_FOUND,
            Json(DispatchRouteResponse::error("dispatch not found")),
        ),
        Err(error) => internal_error(format!("{error}")),
    }
}

/// GET /api/dispatches/:id/events
pub async fn get_dispatch_delivery_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<DispatchRouteResponse>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    if let Err(error) = crate::dispatch::query_dispatch_row_pg(pool, &id).await {
        return if error.to_string().contains("Query returned no rows") {
            (
                StatusCode::NOT_FOUND,
                Json(DispatchRouteResponse::error("dispatch not found")),
            )
        } else {
            internal_error(format!("{error}"))
        };
    }

    match crate::db::dispatches::delivery_events::list_dispatch_delivery_events_pg(pool, &id).await
    {
        Ok(events) => (
            StatusCode::OK,
            Json(DispatchRouteResponse::delivery_events(id, events)),
        ),
        Err(error) => internal_error(format!("{error}")),
    }
}

/// GET /api/dispatches/delivery-events/reconcile-stats
pub async fn get_dispatch_delivery_reconcile_stats(
    State(state): State<AppState>,
) -> (StatusCode, Json<Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    match crate::reconcile::dispatch_delivery_event_reconcile_report_pg(pool).await {
        Ok(report) => (
            StatusCode::OK,
            Json(json!({
                "stats": report.stats,
                "mismatches": report.mismatches,
                "metrics": crate::reconcile::dispatch_delivery_event_mismatch_metrics_snapshot(),
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query dispatch delivery reconcile stats: {error}")})),
        ),
    }
}

/// POST /api/dispatches
pub async fn create_dispatch(
    State(state): State<AppState>,
    Json(body): Json<CreateDispatchBody>,
) -> (StatusCode, Json<DispatchRouteResponse>) {
    let Some(pool) = state.pg_pool_ref() else {
        return pg_unavailable();
    };

    let dispatch_type = body
        .dispatch_type
        .unwrap_or_else(|| "implementation".to_string());
    let to_agent_id = resolve_dispatch_target_agent_id_pg(pool, &body.to_agent_id)
        .await
        .unwrap_or(body.to_agent_id);
    let mut context = body.context.unwrap_or_else(empty_json_object);
    if let Some(required_capabilities) = body.required_capabilities {
        context = context_with_required_capabilities(context, required_capabilities);
    }
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
                Ok(dispatch) => {
                    // #2050 P1 finding 1 — broadcast WS event so other dashboard
                    // clients reflect the new/reused dispatch without manual refresh.
                    let event_name = if reused {
                        "task_dispatch_updated"
                    } else {
                        "task_dispatch_created"
                    };
                    crate::server::ws::emit_event(
                        &state.broadcast_tx,
                        event_name,
                        dispatch.clone(),
                    );
                    (
                        if reused {
                            StatusCode::OK
                        } else {
                            StatusCode::CREATED
                        },
                        Json(DispatchRouteResponse::created_dispatch(dispatch)),
                    )
                }
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
) -> (StatusCode, Json<DispatchRouteResponse>) {
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
                    Json(DispatchRouteResponse::error(
                        "review dispatch completion requires explicit verdict — use POST /api/reviews/verdict",
                    )),
                );
            }

            // #2045 Finding 2 (P2): honor caller-supplied `allowed_from`
            // before delegating to `finalize_dispatch_with_backends`, which
            // hard-codes the gate `["pending","dispatched"]` and silently
            // ignores the caller's intent. This is the only way an external
            // supervisor / recovery loop can guarantee "do not finalize a
            // dispatch that something else already cancelled."
            if let Some(allowed_from) = body.allowed_from.as_ref() {
                let current_status = dispatch
                    .get("status")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let is_allowed = allowed_from
                    .iter()
                    .any(|status| status.as_str() == current_status);
                if !is_allowed {
                    return (
                        StatusCode::CONFLICT,
                        Json(DispatchRouteResponse::error(format!(
                            "dispatch {id} is in status '{current_status}' which is not in allowed_from {allowed_from:?}"
                        ))),
                    );
                }
            }
        }

        return match crate::dispatch::finalize_dispatch_with_backends(
            None,
            &state.engine,
            &id,
            "api",
            body.result.as_ref(),
        ) {
            Ok(dispatch) => {
                // #2050 P1 finding 1 — broadcast task_dispatch_updated on completion.
                crate::server::ws::emit_event(
                    &state.broadcast_tx,
                    "task_dispatch_updated",
                    dispatch.clone(),
                );
                (
                    StatusCode::OK,
                    Json(DispatchRouteResponse::dispatch(dispatch)),
                )
            }
            Err(error) => dispatch_update_error(&id, format!("{error}")),
        };
    }

    if let Some(status) = body.status.as_deref()
        && !VALID_DISPATCH_STATUSES.contains(&status)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(DispatchRouteResponse::error(format!(
                "invalid dispatch status '{}' — allowed values: {}",
                status,
                VALID_DISPATCH_STATUSES.join(", ")
            ))),
        );
    }
    if let Some(allowed_from) = body.allowed_from.as_ref()
        && let Some(invalid) = allowed_from
            .iter()
            .find(|status| !VALID_DISPATCH_STATUSES.contains(&status.as_str()))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(DispatchRouteResponse::error(format!(
                "invalid allowed_from status '{}' — allowed values: {}",
                invalid,
                VALID_DISPATCH_STATUSES.join(", ")
            ))),
        );
    }

    if let Some(status) = body.status {
        let allowed_from_refs = body
            .allowed_from
            .as_ref()
            .map(|statuses| statuses.iter().map(String::as_str).collect::<Vec<_>>());
        let changed = crate::dispatch::set_dispatch_status_with_backends(
            None,
            Some(pool),
            &id,
            &status,
            body.result.as_ref(),
            "api_update_dispatch",
            allowed_from_refs.as_deref(),
            false,
        );
        match changed {
            Ok(0) => {
                if allowed_from_refs.is_some()
                    && let Ok(dispatch) = crate::dispatch::query_dispatch_row_pg(pool, &id).await
                {
                    return (
                        StatusCode::OK,
                        Json(DispatchRouteResponse::dispatch(dispatch)),
                    );
                }
                return (
                    StatusCode::NOT_FOUND,
                    Json(DispatchRouteResponse::error("dispatch not found")),
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
                    Json(DispatchRouteResponse::error("dispatch not found")),
                );
            }
            Ok(_) => {}
            Err(error) => return internal_error(error),
        }
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(DispatchRouteResponse::error("no fields to update")),
        );
    }

    match crate::dispatch::query_dispatch_row_pg(pool, &id).await {
        Ok(dispatch) => {
            // #2050 P1 finding 1 — broadcast task_dispatch_updated on PATCH success.
            crate::server::ws::emit_event(
                &state.broadcast_tx,
                "task_dispatch_updated",
                dispatch.clone(),
            );
            (
                StatusCode::OK,
                Json(DispatchRouteResponse::dispatch(dispatch)),
            )
        }
        Err(error) => internal_error(format!("{error}")),
    }
}

fn pg_unavailable() -> (StatusCode, Json<DispatchRouteResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(DispatchRouteResponse::error("postgres pool unavailable")),
    )
}

fn empty_json_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

fn context_with_required_capabilities(
    mut context: serde_json::Value,
    required_capabilities: serde_json::Value,
) -> serde_json::Value {
    if let Some(obj) = context.as_object_mut() {
        obj.insert("required_capabilities".to_string(), required_capabilities);
        context
    } else {
        let mut wrapped = serde_json::Map::new();
        wrapped.insert("value".to_string(), context);
        wrapped.insert("required_capabilities".to_string(), required_capabilities);
        serde_json::Value::Object(wrapped)
    }
}

fn internal_error(error: impl Into<String>) -> (StatusCode, Json<DispatchRouteResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(DispatchRouteResponse::error(error)),
    )
}

fn dispatch_create_error(message: String) -> (StatusCode, Json<DispatchRouteResponse>) {
    if message.contains("not found") {
        (
            StatusCode::NOT_FOUND,
            Json(DispatchRouteResponse::error(message)),
        )
    } else if message.starts_with("Cannot create ") || message.contains("already exists") {
        (
            StatusCode::CONFLICT,
            Json(DispatchRouteResponse::error(message)),
        )
    } else {
        internal_error(message)
    }
}

fn dispatch_update_error(
    dispatch_id: &str,
    message: String,
) -> (StatusCode, Json<DispatchRouteResponse>) {
    if message.contains("not found") {
        (
            StatusCode::NOT_FOUND,
            Json(DispatchRouteResponse::dispatch_error(message, dispatch_id)),
        )
    } else if message.contains("no agent execution evidence") {
        (
            StatusCode::BAD_REQUEST,
            Json(DispatchRouteResponse::dispatch_error(message, dispatch_id)),
        )
    } else {
        internal_error(message)
    }
}

async fn list_dispatches_pg(
    pool: &sqlx::PgPool,
    status: Option<&str>,
    kanban_card_id: Option<&str>,
    from_agent_id: Option<&str>,
    to_agent_id: Option<&str>,
    limit: Option<i64>,
) -> Result<Vec<DispatchListItem>, String> {
    // #2050 P2 finding 4 — honor optional filter + bounded limit.
    let bounded_limit = limit
        .map(|value| value.max(1).min(1_000))
        .unwrap_or(1_000);
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
           AND ($3::text IS NULL OR from_agent_id = $3)
           AND ($4::text IS NULL OR to_agent_id = $4)
         ORDER BY created_at DESC
         LIMIT $5",
    )
    .bind(status)
    .bind(kanban_card_id)
    .bind(from_agent_id)
    .bind(to_agent_id)
    .bind(bounded_limit)
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

            Ok(DispatchListItem {
                id: row
                    .try_get::<String, _>("id")
                    .map_err(|error| format!("decode postgres dispatch id: {error}"))?,
                kanban_card_id: row
                    .try_get::<Option<String>, _>("kanban_card_id")
                    .map_err(|error| format!("decode postgres dispatch kanban_card_id: {error}"))?,
                from_agent_id: row
                    .try_get::<Option<String>, _>("from_agent_id")
                    .map_err(|error| format!("decode postgres dispatch from_agent_id: {error}"))?,
                to_agent_id: row
                    .try_get::<Option<String>, _>("to_agent_id")
                    .map_err(|error| format!("decode postgres dispatch to_agent_id: {error}"))?,
                dispatch_type,
                status,
                title: row
                    .try_get::<Option<String>, _>("title")
                    .map_err(|error| format!("decode postgres dispatch title: {error}"))?,
                context,
                result,
                context_file: None,
                result_file: None,
                result_summary,
                parent_dispatch_id: row
                    .try_get::<Option<String>, _>("parent_dispatch_id")
                    .map_err(|error| {
                        format!("decode postgres dispatch parent_dispatch_id: {error}")
                    })?,
                chain_depth: row
                    .try_get::<i64, _>("chain_depth")
                    .map_err(|error| format!("decode postgres dispatch chain_depth: {error}"))?,
                created_at: created_at.clone(),
                dispatched_at: Some(created_at),
                updated_at,
                completed_at,
            })
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

/// PATCH /api/dispatches/:id with only `result` supplied.
///
/// #2045 Finding 1 (P1): the previous implementation issued a bare
/// `UPDATE task_dispatches SET result = ..., updated_at = NOW()` and skipped
/// the rest of the dispatch lifecycle pipeline. That meant phase-gate sidecars
/// that PATCHed `{"checks":{...}}` were stuck — the durable phase-gate row
/// never picked up the new verdict because
/// `reconcile_phase_gate_for_terminal_dispatch_on_pg_tx` runs only inside a
/// terminal status transition, and there was no audit row in `dispatch_events`
/// so incident-response could not see the result mutation. We now perform the
/// update inside a transaction together with:
///  1. an audit `dispatch_events` row (from_status=to_status=current, source
///     `api_update_result`), and
///  2. a phase-gate reconciliation pass scoped to the current status when the
///     dispatch is already terminal — so a late PATCH of verdict / `checks`
///     propagates into the durable phase-gate state instead of being stranded.
async fn update_dispatch_result_pg(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<usize, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres update-result transaction: {error}"))?;

    let current = sqlx::query(
        "SELECT status, kanban_card_id, dispatch_type, context::TEXT AS context_text
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| format!("load postgres dispatch {dispatch_id} for result update: {error}"))?;
    let Some(current) = current else {
        let _ = tx.rollback().await;
        return Ok(0);
    };

    let current_status: Option<String> = current
        .try_get("status")
        .map_err(|error| format!("decode dispatch status for {dispatch_id}: {error}"))?;
    let kanban_card_id: Option<String> = current
        .try_get("kanban_card_id")
        .map_err(|error| format!("decode dispatch kanban_card_id for {dispatch_id}: {error}"))?;
    let dispatch_type: Option<String> = current
        .try_get("dispatch_type")
        .map_err(|error| format!("decode dispatch type for {dispatch_id}: {error}"))?;
    let context_text: Option<String> = current
        .try_get("context_text")
        .map_err(|error| format!("decode dispatch context for {dispatch_id}: {error}"))?;

    let result_json = serde_json::to_string(result)
        .map_err(|error| format!("serialize dispatch result {dispatch_id}: {error}"))?;

    let updated = sqlx::query(
        "UPDATE task_dispatches
         SET result = CAST($2 AS jsonb),
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .bind(&result_json)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("update postgres dispatch result {dispatch_id}: {error}"))?;

    if updated.rows_affected() == 0 {
        let _ = tx.rollback().await;
        return Ok(0);
    }

    let status_for_audit = current_status.clone().unwrap_or_default();
    sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES ($1, $2, $3, $4, $5, 'api_update_result', CAST($6 AS jsonb))",
    )
    .bind(dispatch_id)
    .bind(&kanban_card_id)
    .bind(&dispatch_type)
    .bind(&status_for_audit)
    .bind(&status_for_audit)
    .bind(&result_json)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("insert dispatch_events audit row for {dispatch_id}: {error}"))?;

    if matches!(
        current_status.as_deref(),
        Some("completed" | "failed" | "cancelled")
    ) {
        let _ = crate::db::auto_queue::reconcile_phase_gate_for_terminal_dispatch_on_pg_tx(
            &mut tx,
            dispatch_id,
            current_status.as_deref().unwrap_or("completed"),
            context_text.as_deref(),
            Some(&result_json),
        )
        .await
        .map_err(|error| {
            format!("reconcile phase-gate after result PATCH {dispatch_id}: {error}")
        })?;
    }

    tx.commit().await.map_err(|error| {
        format!("commit postgres update-result transaction {dispatch_id}: {error}")
    })?;

    Ok(updated.rows_affected() as usize)
}

#[cfg(test)]
mod tests {
    use super::{get_dispatch_delivery_events, get_dispatch_delivery_reconcile_stats};
    use axum::extract::{Path, State};
    use axum::http::StatusCode;

    struct TestPostgresDb {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name = format!(
                "agentdesk_dispatch_route_events_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "dispatch delivery events route test",
            )
            .await
            .unwrap();

            Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn try_create() -> Option<Self> {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name = format!(
                "agentdesk_dispatch_route_events_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            if let Err(error) = crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "dispatch delivery events route test",
            )
            .await
            {
                eprintln!("skipping postgres-backed dispatch route test: {error}");
                drop(lock);
                return None;
            }

            Some(Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "dispatch delivery events route test",
            )
            .await
            .unwrap()
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "dispatch delivery events route test",
            )
            .await
            .unwrap();
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    fn test_engine_with_pg(pg_pool: sqlx::PgPool) -> crate::engine::PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        crate::engine::PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
    }

    fn test_state_with_pg(pg_pool: sqlx::PgPool) -> crate::server::routes::AppState {
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        crate::server::routes::AppState {
            #[cfg(all(test, feature = "legacy-sqlite-tests"))]
            legacy_db_override: None,
            pg_pool: Some(pg_pool.clone()),
            engine: test_engine_with_pg(pg_pool),
            config: std::sync::Arc::new(crate::config::Config::default()),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
            cluster_instance_id: None,
        }
    }

    #[tokio::test]
    async fn dispatch_delivery_events_route_returns_typed_rows_newest_first() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let state = test_state_with_pg(pool.clone());

        sqlx::query(
            "INSERT INTO task_dispatches (id, status, title, created_at, updated_at)
             VALUES ($1, 'completed', 'Delivery event route test', NOW(), NOW())",
        )
        .bind("dispatch-events-route")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO dispatch_delivery_events (
                dispatch_id, correlation_id, semantic_event_id, operation, target_kind,
                status, attempt, error, result_json, created_at, updated_at
             ) VALUES (
                $1, 'dispatch:dispatch-events-route', 'dispatch:dispatch-events-route:notify',
                'send', 'channel', 'failed', 1, 'first failure',
                '{\"status\":\"failed\"}'::jsonb,
                NOW() - INTERVAL '1 minute',
                NOW() - INTERVAL '1 minute'
             )",
        )
        .bind("dispatch-events-route")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO dispatch_delivery_events (
                dispatch_id, correlation_id, semantic_event_id, operation, target_kind,
                target_channel_id, status, attempt, message_id, messages_json, result_json,
                created_at, updated_at
             ) VALUES (
                $1, 'dispatch:dispatch-events-route', 'dispatch:dispatch-events-route:notify',
                'send', 'channel', '1500000000000000000', 'sent', 2,
                '1500000000000000001',
                '[{\"channel_id\":\"1500000000000000000\",\"message_id\":\"1500000000000000001\"}]'::jsonb,
                '{\"status\":\"success\"}'::jsonb, NOW(), NOW()
             )",
        )
        .bind("dispatch-events-route")
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) =
            get_dispatch_delivery_events(State(state), Path("dispatch-events-route".to_string()))
                .await;
        let body = serde_json::to_value(body.0).unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["dispatch_id"], "dispatch-events-route");
        assert_eq!(body["events"].as_array().unwrap().len(), 2);
        assert_eq!(body["events"][0]["status"], "sent");
        assert_eq!(body["events"][0]["attempt"], 2);
        assert_eq!(body["events"][0]["message_id"], "1500000000000000001");
        assert_eq!(body["events"][1]["status"], "failed");
        assert_eq!(body["events"][1]["error"], "first failure");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn dispatch_delivery_reconcile_stats_route_returns_current_stats_and_metric_rows() {
        crate::reconcile::reset_dispatch_delivery_event_mismatch_metrics_for_tests();
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let state = test_state_with_pg(pool.clone());

        sqlx::query(
            "INSERT INTO task_dispatches (id, status, title)
             VALUES ($1, 'pending', 'Delivery reconcile route test')",
        )
        .bind("dispatch-route-reconcile")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ('dispatch_reserving:dispatch-route-reconcile', 'dispatch-route-reconcile')",
        )
        .execute(&pool)
        .await
        .unwrap();
        crate::reconcile::reconcile_dispatch_delivery_events_pg(&pool)
            .await
            .unwrap();

        let (status, body) = get_dispatch_delivery_reconcile_stats(State(state)).await;
        let body = body.0;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["stats"]["mismatch_count"], 1);
        assert_eq!(body["stats"]["missing_typed"], 1);
        assert_eq!(
            body["mismatches"][0]["dispatch_id"],
            "dispatch-route-reconcile"
        );
        assert_eq!(
            body["metrics"][0]["name"],
            "agentdesk_dispatch_delivery_event_mismatch_total"
        );
        assert_eq!(body["metrics"][0]["kind"], "missing_typed");
        assert_eq!(body["metrics"][0]["value"], 1);

        pool.close().await;
        pg_db.drop().await;
    }
}
