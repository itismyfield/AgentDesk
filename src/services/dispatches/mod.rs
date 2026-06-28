use serde_json::{Value, json};
use sqlx::Row;

use crate::dispatch;
use crate::engine::PolicyEngine;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};

// #1693: Discord delivery transport / error types / delivery-guard helper
// migrated from `crate::services::discord_delivery` (the flat path is kept
// as a re-export in `services/mod.rs` for compatibility).
pub(crate) mod discord_delivery;
pub(crate) mod thread_reuse;

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

// #3037: dispatch loopback request DTO (`UpdateDispatchBody`) relocated here
// from `crate::server::routes::dispatches::crud` so the dependency direction is
// server → services. Re-exported below for the existing call sites.
pub mod dtos;
pub use dtos::UpdateDispatchBody;

#[derive(Clone)]
pub struct DispatchService {
    engine: PolicyEngine,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct LinkDispatchThreadBody {
    pub dispatch_id: String,
    pub thread_id: String,
    pub channel_id: Option<String>,
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
