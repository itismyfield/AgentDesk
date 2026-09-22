use crate::services::cluster::agent_execution_node::{self, AgentExecutionNode};
use crate::services::cluster::execution_requirements::{self, ExecutionRequirements};
use crate::{
    app_state::AppState,
    error::{AppError, AppResult},
};
use axum::{
    Json,
    extract::{Path, State},
};
use serde_json::{Value, json};

pub(super) async fn get_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let pool = state
        .pg_pool_ref()
        .ok_or_else(|| AppError::internal("postgres unavailable"))?;
    let node = agent_execution_node::get(pool, &id)
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
        .ok_or_else(|| AppError::not_found("agent not found"))?;
    Ok(Json(json!({
        "default_node_id": node.default_node_id,
        "routing_enforced": crate::services::cluster::intake_routing_config::effective_intake_routing_config().mode_is_enforce(),
    })))
}

pub(super) async fn put_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(policy): Json<AgentExecutionNode>,
) -> AppResult<Json<Value>> {
    policy.validate().map_err(AppError::bad_request)?;
    let pool = state
        .pg_pool_ref()
        .ok_or_else(|| AppError::internal("postgres unavailable"))?;
    if let Some(node) = &policy.default_node_id {
        if !crate::services::cluster::intake_routing_config::effective_intake_routing_config()
            .mode_is_enforce()
        {
            return Err(AppError::bad_request(
                "default execution node requires enforce intake routing",
            ));
        }
        let exists = agent_execution_node::node_registered(pool, node)
            .await
            .map_err(|e| AppError::internal(e.to_string()))?;
        if !exists {
            return Err(AppError::bad_request("unknown node instance ID"));
        }
    }
    let updated = agent_execution_node::set(pool, &id, &policy)
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
    if !updated {
        return Err(AppError::not_found("agent not found"));
    }
    Ok(Json(json!({"default_node_id": policy.default_node_id})))
}

pub(super) async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let pool = state
        .pg_pool_ref()
        .ok_or_else(|| AppError::internal("postgres unavailable"))?;
    let policy = execution_requirements::get(pool, &id)
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
    Ok(Json(
        json!({"execution_requirements":policy.ok_or_else(|| AppError::not_found("agent not found"))?}),
    ))
}

pub(super) async fn put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(value): Json<Value>,
) -> AppResult<Json<Value>> {
    let policy = ExecutionRequirements::parse(value).map_err(AppError::bad_request)?;
    let value = serde_json::to_value(policy).map_err(|e| AppError::internal(e.to_string()))?;
    let pool = state
        .pg_pool_ref()
        .ok_or_else(|| AppError::internal("postgres unavailable"))?;
    let updated = execution_requirements::set(pool, &id, &value)
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
    if !updated {
        return Err(AppError::not_found("agent not found"));
    }
    Ok(Json(json!({"execution_requirements":value})))
}
