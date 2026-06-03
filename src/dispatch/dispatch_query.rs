use anyhow::Result;
use serde_json::json;
use sqlx::{PgPool, Row};

use super::dispatch_summary::{parse_dispatch_json_text, summarize_dispatch_result};

pub(crate) async fn query_dispatch_row_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<serde_json::Value> {
    let row = sqlx::query(
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
            COALESCE(chain_depth, 0)::bigint AS chain_depth,
            created_at::text AS created_at,
            updated_at::text AS updated_at,
            completed_at::text AS completed_at,
            COALESCE(retry_count, 0)::bigint AS retry_count,
            required_capabilities,
            routing_diagnostics,
            constraint_results
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?
    .ok_or_else(|| anyhow::anyhow!("Dispatch query error: Query returned no rows"))?;

    let status = row
        .try_get::<String, _>("status")
        .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?;
    let updated_at = row
        .try_get::<String, _>("updated_at")
        .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?;
    let dispatch_type = row
        .try_get::<Option<String>, _>("dispatch_type")
        .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?;
    let context_raw = row
        .try_get::<Option<String>, _>("context")
        .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?;
    let result_raw = row
        .try_get::<Option<String>, _>("result")
        .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?;
    let context = parse_dispatch_json_text(context_raw.as_deref());
    let result = parse_dispatch_json_text(result_raw.as_deref());
    let result_summary = summarize_dispatch_result(
        dispatch_type.as_deref(),
        Some(status.as_str()),
        result.as_ref(),
        context.as_ref(),
    );
    let completed_at = row
        .try_get::<Option<String>, _>("completed_at")
        .map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?
        .or_else(|| (status == "completed").then(|| updated_at.clone()));

    Ok(json!({
        "id": row.try_get::<String, _>("id").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "kanban_card_id": row.try_get::<Option<String>, _>("kanban_card_id").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "from_agent_id": row.try_get::<Option<String>, _>("from_agent_id").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "to_agent_id": row.try_get::<Option<String>, _>("to_agent_id").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "dispatch_type": dispatch_type,
        "status": status,
        "title": row.try_get::<Option<String>, _>("title").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "context": context,
        "result": result,
        "result_summary": result_summary,
        "parent_dispatch_id": row.try_get::<Option<String>, _>("parent_dispatch_id").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "chain_depth": row.try_get::<i64, _>("chain_depth").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "created_at": row.try_get::<String, _>("created_at").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "updated_at": updated_at,
        "completed_at": completed_at,
        "retry_count": row.try_get::<i64, _>("retry_count").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "required_capabilities": row.try_get::<Option<serde_json::Value>, _>("required_capabilities").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "routing_diagnostics": row.try_get::<Option<serde_json::Value>, _>("routing_diagnostics").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
        "constraint_results": row.try_get::<Option<serde_json::Value>, _>("constraint_results").map_err(|error| anyhow::anyhow!("Dispatch query error: {error}"))?,
    }))
}
