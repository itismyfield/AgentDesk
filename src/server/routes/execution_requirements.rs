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
