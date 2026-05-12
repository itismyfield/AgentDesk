use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::Row;

use crate::dispatch;
use crate::engine::PolicyEngine;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};

// #1693: Discord delivery transport / error types / delivery-guard helper
// migrated from `crate::services::discord_delivery` (the flat path is kept
// as a re-export in `services/mod.rs` for compatibility).
pub(crate) mod discord_delivery;

// #1730: Claim-owner capability matching and routing diagnostics semantics
// live in the service layer; DB modules only select/mark/persist.
pub(crate) mod outbox_claiming;

// #1694: Dispatch outbox queue worker + state-transition logic, extracted
// from `src/server/routes/dispatches/outbox.rs`. See module doc for the
// route ↔ service ↔ db boundary.
pub(crate) mod outbox_queue;
pub(crate) mod outbox_route;
pub(crate) mod routing_constraint;
pub(crate) mod wait_queue;

const VALID_DISPATCH_STATUSES: &[&str] =
    &["pending", "dispatched", "completed", "cancelled", "failed"];

#[derive(Clone)]
pub struct DispatchService {
    engine: PolicyEngine,
}

pub struct CreateDispatchInput {
    pub kanban_card_id: String,
    pub to_agent_id: String,
    pub dispatch_type: Option<String>,
    pub title: String,
    pub context: Option<Value>,
    pub skip_outbox: Option<bool>,
}

pub struct CreateDispatchResult {
    pub dispatch: Value,
    pub status: StatusCode,
}

pub struct UpdateDispatchInput {
    pub status: Option<String>,
    pub result: Option<Value>,
}

impl DispatchService {
    pub fn new(engine: PolicyEngine) -> Self {
        Self { engine }
    }

    pub fn list_dispatches(
        &self,
        status: Option<&str>,
        kanban_card_id: Option<&str>,
    ) -> ServiceResult<Vec<Value>> {
        let pool = self.engine.pg_pool().ok_or_else(|| {
            ServiceError::internal("Postgres pool required to list dispatches")
                .with_code(ErrorCode::Database)
                .with_operation("list_dispatches.pg")
        })?;
        list_dispatches_pg(pool, status, kanban_card_id).map_err(|error| {
            ServiceError::internal(error)
                .with_code(ErrorCode::Database)
                .with_operation("list_dispatches.pg")
        })
    }

    pub fn get_dispatch(&self, id: &str) -> ServiceResult<Value> {
        dispatch::load_dispatch_row_with_backends(None, self.engine.pg_pool(), id)
            .map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("get_dispatch.query")
                    .with_context("dispatch_id", id)
            })?
            .ok_or_else(|| {
                ServiceError::not_found("dispatch not found")
                    .with_code(ErrorCode::Dispatch)
                    .with_context("dispatch_id", id)
            })
    }

    pub async fn create_dispatch(
        &self,
        input: CreateDispatchInput,
    ) -> ServiceResult<CreateDispatchResult> {
        let dispatch_type = input
            .dispatch_type
            .unwrap_or_else(|| "implementation".to_string());
        let to_agent_id = if let Some(pg_pool) = self.engine.pg_pool() {
            resolve_dispatch_target_agent_id_with_pg(pg_pool, &input.to_agent_id)
                .await
                .unwrap_or_else(|| input.to_agent_id.clone())
        } else {
            input.to_agent_id.clone()
        };
        let context = input.context.unwrap_or_else(|| json!({}));
        let options = dispatch::DispatchCreateOptions {
            skip_outbox: input.skip_outbox.unwrap_or(false),
            sidecar_dispatch: context
                .get("sidecar_dispatch")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
                || context
                    .get("phase_gate")
                    .and_then(|value| value.as_object())
                    .is_some(),
        };

        let pool = self.engine.pg_pool().ok_or_else(|| {
            ServiceError::internal("Postgres pool required to create dispatch")
                .with_code(ErrorCode::Database)
                .with_operation("create_dispatch.pg_pool")
        })?;
        let result = dispatch::create_dispatch_with_options_pg_only(
            pool,
            &self.engine,
            &input.kanban_card_id,
            &to_agent_id,
            &dispatch_type,
            &input.title,
            &context,
            options,
        );

        match result {
            Ok(dispatch) => {
                let was_reused = dispatch
                    .get("__reused")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                Ok(CreateDispatchResult {
                    dispatch,
                    status: if was_reused {
                        StatusCode::OK
                    } else {
                        StatusCode::CREATED
                    },
                })
            }
            Err(error) => {
                let message = format!("{error}");
                if message.contains("not found") {
                    Err(ServiceError::not_found(message).with_code(ErrorCode::Dispatch))
                } else if message.starts_with("Cannot create ")
                    || message.contains("already exists")
                {
                    Err(ServiceError::conflict(message).with_code(ErrorCode::Dispatch))
                } else {
                    Err(ServiceError::internal(message).with_code(ErrorCode::Dispatch))
                }
            }
        }
    }

    pub fn update_dispatch(&self, id: &str, input: UpdateDispatchInput) -> ServiceResult<Value> {
        if input.status.as_deref() == Some("completed") {
            let context = input.result.as_ref();

            // Guard: review dispatches MUST carry an explicit verdict.
            // /api/reviews/verdict validates this; this path (PATCH
            // /api/dispatches/:id) previously accepted `{items, notes}` without
            // a verdict, leaving review_state in `reviewing` until timeouts [C]
            // escalated the card to dilemma_pending (see #925 incident).
            if let Ok(Some(row)) =
                crate::dispatch::load_dispatch_row_with_backends(None, self.engine.pg_pool(), id)
            {
                let dtype = row
                    .get("dispatch_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if dtype == "review" {
                    let has_verdict = context
                        .and_then(|c| c.get("verdict").or_else(|| c.get("decision")))
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.is_empty());
                    if !has_verdict {
                        return Err(ServiceError::bad_request(
                            "review dispatch completion requires explicit verdict — use POST /api/reviews/verdict",
                        )
                        .with_code(ErrorCode::Validation)
                        .with_context("dispatch_id", id));
                    }
                }
            }

            let dispatch =
                dispatch::finalize_dispatch_with_backends(None, &self.engine, id, "api", context)
                    .map_err(|error| {
                    let message = format!("{error}");
                    if message.contains("not found") {
                        ServiceError::not_found(message)
                            .with_code(ErrorCode::Dispatch)
                            .with_context("dispatch_id", id)
                    } else if message.contains("no agent execution evidence") {
                        ServiceError::bad_request(message)
                            .with_code(ErrorCode::Dispatch)
                            .with_context("dispatch_id", id)
                    } else {
                        ServiceError::internal(message)
                            .with_code(ErrorCode::Dispatch)
                            .with_context("dispatch_id", id)
                    }
                })?;
            if let Some(pg_pool) = self.engine.pg_pool() {
                let dispatch_id = id.to_string();
                if let Err(error) = crate::utils::async_bridge::block_on_pg_result(
                    pg_pool,
                    move |bridge_pool| async move {
                        crate::services::dispatches_followup::queue_dispatch_followup_pg(
                            &bridge_pool,
                            &dispatch_id,
                        )
                        .await
                    },
                    |error| error,
                ) {
                    tracing::warn!(
                        dispatch_id = %id,
                        "failed to enqueue postgres followup: {error}"
                    );
                }
            }
            return Ok(dispatch);
        }

        if let Some(status) = input.status.as_deref()
            && !VALID_DISPATCH_STATUSES.contains(&status)
        {
            return Err(ServiceError::bad_request(format!(
                "invalid dispatch status '{}' — allowed values: {}",
                status,
                VALID_DISPATCH_STATUSES.join(", ")
            ))
            .with_code(ErrorCode::Validation)
            .with_context("dispatch_id", id)
            .with_context("status", status));
        }

        if let Some(status) = input.status {
            let changed = dispatch::set_dispatch_status_with_backends(
                None,
                self.engine.pg_pool(),
                id,
                &status,
                input.result.as_ref(),
                "api_update_dispatch",
                None,
                false,
            )
            .map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Dispatch)
                    .with_operation("update_dispatch.set_status")
                    .with_context("dispatch_id", id)
            })?;
            if changed == 0 {
                return Err(ServiceError::not_found("dispatch not found")
                    .with_code(ErrorCode::Dispatch)
                    .with_context("dispatch_id", id));
            }
        } else if let Some(result) = input.result {
            let pool = self.engine.pg_pool().ok_or_else(|| {
                ServiceError::internal("Postgres pool required to update dispatch result")
                    .with_code(ErrorCode::Database)
                    .with_operation("update_dispatch.update_result_pg")
                    .with_context("dispatch_id", id)
            })?;
            let changed = update_dispatch_result_pg(pool, id, &result).map_err(|error| {
                ServiceError::internal(error)
                    .with_code(ErrorCode::Database)
                    .with_operation("update_dispatch.update_result_pg")
                    .with_context("dispatch_id", id)
            })?;

            if changed == 0 {
                return Err(ServiceError::not_found("dispatch not found")
                    .with_code(ErrorCode::Dispatch)
                    .with_context("dispatch_id", id));
            }
        } else {
            return Err(ServiceError::bad_request("no fields to update")
                .with_code(ErrorCode::Validation)
                .with_context("dispatch_id", id));
        }

        dispatch::load_dispatch_row_with_backends(None, self.engine.pg_pool(), id)
            .map_err(|error| {
                ServiceError::internal(format!("{error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("update_dispatch.readback")
                    .with_context("dispatch_id", id)
            })?
            .ok_or_else(|| {
                ServiceError::not_found("dispatch not found")
                    .with_code(ErrorCode::Dispatch)
                    .with_context("dispatch_id", id)
            })
    }
}

fn list_dispatches_pg(
    pool: &sqlx::PgPool,
    status: Option<&str>,
    kanban_card_id: Option<&str>,
) -> Result<Vec<Value>, String> {
    let status = status.map(|value| value.to_string());
    let kanban_card_id = kanban_card_id.map(|value| value.to_string());
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |pool| async move {
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
            .bind(status.as_deref())
            .bind(kanban_card_id.as_deref())
            .fetch_all(&pool)
            .await
            .map_err(|error| format!("list postgres dispatches: {error}"))?;

            rows.into_iter()
                .map(|row| dispatch_row_to_json_pg(&row))
                .collect()
        },
        |error| error,
    )
}

/// Service-layer twin of `crud.rs::update_dispatch_result_pg`.
///
/// #2045 Finding 1 (P1): see the route handler doc for the rationale. We run
/// the update inside a transaction, emit a `dispatch_events` audit row, and
/// reconcile phase-gate state when the dispatch is already terminal.
fn update_dispatch_result_pg(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
    result: &Value,
) -> Result<usize, String> {
    let dispatch_id = dispatch_id.to_string();
    let result_json = serde_json::to_string(result)
        .map_err(|error| format!("serialize dispatch result {dispatch_id}: {error}"))?;
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |pool| async move {
            let mut tx = pool.begin().await.map_err(|error| {
                format!("begin postgres update-result transaction {dispatch_id}: {error}")
            })?;

            let current = sqlx::query(
                "SELECT status, kanban_card_id, dispatch_type, context::TEXT AS context_text
                 FROM task_dispatches
                 WHERE id = $1",
            )
            .bind(&dispatch_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| {
                format!("load postgres dispatch {dispatch_id} for result update: {error}")
            })?;
            let Some(current) = current else {
                let _ = tx.rollback().await;
                return Ok(0);
            };

            let current_status: Option<String> = current
                .try_get("status")
                .map_err(|error| format!("decode dispatch status for {dispatch_id}: {error}"))?;
            let kanban_card_id: Option<String> =
                current.try_get("kanban_card_id").map_err(|error| {
                    format!("decode dispatch kanban_card_id for {dispatch_id}: {error}")
                })?;
            let dispatch_type: Option<String> = current
                .try_get("dispatch_type")
                .map_err(|error| format!("decode dispatch type for {dispatch_id}: {error}"))?;
            let context_text: Option<String> = current
                .try_get("context_text")
                .map_err(|error| format!("decode dispatch context for {dispatch_id}: {error}"))?;

            let updated = sqlx::query(
                "UPDATE task_dispatches
                 SET result = CAST($2 AS jsonb),
                     updated_at = NOW()
                 WHERE id = $1",
            )
            .bind(&dispatch_id)
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
            .bind(&dispatch_id)
            .bind(&kanban_card_id)
            .bind(&dispatch_type)
            .bind(&status_for_audit)
            .bind(&status_for_audit)
            .bind(&result_json)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                format!("insert dispatch_events audit row for {dispatch_id}: {error}")
            })?;

            if matches!(
                current_status.as_deref(),
                Some("completed" | "failed" | "cancelled")
            ) {
                let _ = crate::db::auto_queue::reconcile_phase_gate_for_terminal_dispatch_on_pg_tx(
                    &mut tx,
                    &dispatch_id,
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
        },
        |error| error,
    )
}

async fn resolve_dispatch_target_agent_id_with_pg(
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

fn dispatch_row_to_json_pg(row: &sqlx::postgres::PgRow) -> Result<Value, String> {
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
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let result = result_raw
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let result_summary = dispatch::summarize_dispatch_result(
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
        "context_file": Value::Null,
        "result_file": Value::Null,
        "result_summary": result_summary,
        "parent_dispatch_id": row.try_get::<Option<String>, _>("parent_dispatch_id").map_err(|error| format!("decode postgres dispatch parent_dispatch_id: {error}"))?,
        "chain_depth": row.try_get::<i64, _>("chain_depth").map_err(|error| format!("decode postgres dispatch chain_depth: {error}"))?,
        "created_at": created_at.clone(),
        "dispatched_at": Some(created_at),
        "updated_at": updated_at,
        "completed_at": completed_at,
    }))
}
