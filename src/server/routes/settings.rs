use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;
use serde_json::Value;

use super::AppState;
use crate::error::AppResult;
use crate::services::operator_connectors::OptionalConnectorsResponse;

fn settings_json_response<T: Serialize>(status: StatusCode, body: T) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(serde_json::to_value(body).expect("settings response DTO serializes")),
    )
}

/// GET /api/settings
pub async fn get_settings(State(state): State<AppState>) -> AppResult<(StatusCode, Json<Value>)> {
    match state.settings_service().get_settings().await {
        Ok(body) => Ok(settings_json_response(StatusCode::OK, body)),
        Err(error) => Err(error),
    }
}

/// PUT /api/settings
/// Replaces the stored `kv_meta['settings']` JSON object; callers must send a merged payload
/// if they want to preserve hidden keys. Retired legacy settings keys are stripped server-side.
pub async fn put_settings(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> AppResult<(StatusCode, Json<Value>)> {
    match state.settings_service().put_settings(body).await {
        Ok(body) => Ok(settings_json_response(StatusCode::OK, body)),
        Err(error) => Err(error),
    }
}

/// GET /api/settings/config
/// Returns each whitelisted key with its effective value, baseline, mutability, and
/// restart-behavior metadata so callers can distinguish baseline from live override.
pub async fn get_config_entries(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<Value>)> {
    match state.settings_service().get_config_entries().await {
        Ok(body) => Ok(settings_json_response(StatusCode::OK, body)),
        Err(error) => Err(error),
    }
}

/// PATCH /api/settings/config
/// Writes live overrides for editable whitelisted keys only. Read-only metadata entries
/// such as `server_port` are rejected instead of being persisted as misleading overrides.
pub async fn patch_config_entries(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> AppResult<(StatusCode, Json<Value>)> {
    match state.settings_service().patch_config_entries(body).await {
        Ok(body) => Ok(settings_json_response(StatusCode::OK, body)),
        Err(error) => Err(error),
    }
}

/// GET /api/settings/runtime-config
pub async fn get_runtime_config(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<Value>)> {
    match state.settings_service().get_runtime_config().await {
        Ok(body) => Ok(settings_json_response(StatusCode::OK, body)),
        Err(error) => Err(error),
    }
}

/// GET /api/settings/operator-connectors
pub async fn get_operator_connectors() -> AppResult<(StatusCode, Json<Value>)> {
    Ok(settings_json_response(
        StatusCode::OK,
        OptionalConnectorsResponse::current(),
    ))
}

/// PUT /api/settings/runtime-config applies a metadata-less update or explicit replacement.
/// A supplied `__runtimeConfigExplicitKeys` list is authoritative (including empty); without
/// it, known submitted keys become explicit and existing explicit overrides are retained.
pub async fn put_runtime_config(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> AppResult<(StatusCode, Json<Value>)> {
    match state.settings_service().put_runtime_config(body).await {
        Ok(body) => Ok(settings_json_response(StatusCode::OK, body)),
        Err(error) => Err(error),
    }
}
