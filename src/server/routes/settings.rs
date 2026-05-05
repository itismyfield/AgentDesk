use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;
use serde_json::Value;

use super::AppState;
use crate::error::AppError;
use crate::server::dto::settings::SettingsErrorResponse;

fn settings_json_response<T: Serialize>(status: StatusCode, body: T) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(serde_json::to_value(body).expect("settings response DTO serializes")),
    )
}

fn service_error_response(error: AppError) -> (StatusCode, Json<Value>) {
    let status = error.status();
    settings_json_response(
        status,
        SettingsErrorResponse {
            error: error.message(),
        },
    )
}

/// GET /api/settings
pub async fn get_settings(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    match state.settings_service().get_settings().await {
        Ok(body) => settings_json_response(StatusCode::OK, body),
        Err(error) => service_error_response(error),
    }
}

/// PUT /api/settings
/// Replaces the stored `kv_meta['settings']` JSON object; callers must send a merged payload
/// if they want to preserve hidden keys. Retired legacy settings keys are stripped server-side.
pub async fn put_settings(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    match state.settings_service().put_settings(body).await {
        Ok(body) => settings_json_response(StatusCode::OK, body),
        Err(error) => service_error_response(error),
    }
}

/// GET /api/settings/config
/// Returns each whitelisted key with its effective value, baseline, mutability, and
/// restart-behavior metadata so callers can distinguish baseline from live override.
pub async fn get_config_entries(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    match state.settings_service().get_config_entries().await {
        Ok(body) => settings_json_response(StatusCode::OK, body),
        Err(error) => service_error_response(error),
    }
}

/// PATCH /api/settings/config
/// Writes live overrides for editable whitelisted keys only. Read-only metadata entries
/// such as `server_port` are rejected instead of being persisted as misleading overrides.
pub async fn patch_config_entries(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    match state.settings_service().patch_config_entries(body).await {
        Ok(body) => settings_json_response(StatusCode::OK, body),
        Err(error) => service_error_response(error),
    }
}

/// GET /api/settings/runtime-config
pub async fn get_runtime_config(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    match state.settings_service().get_runtime_config().await {
        Ok(body) => settings_json_response(StatusCode::OK, body),
        Err(error) => service_error_response(error),
    }
}

/// PUT /api/settings/runtime-config
pub async fn put_runtime_config(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    match state.settings_service().put_runtime_config(body).await {
        Ok(body) => settings_json_response(StatusCode::OK, body),
        Err(error) => service_error_response(error),
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[path = "settings_tests.rs"]
mod tests;
