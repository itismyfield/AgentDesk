//! `GET /api/prompt-manifest/retention` (#1699).
//!
//! Surfaces the current storage cost of the `prompt_manifests` /
//! `prompt_manifest_layers` tables plus the retention-policy snapshot from
//! `agentdesk.yaml::prompt_manifest_retention`.
//!
//! Response shape:
//! ```json
//! {
//!   "total_stored_bytes": 1234,
//!   "total_original_bytes": 5678,
//!   "manifest_count": 42,
//!   "layer_count": 168,
//!   "truncated_count": 3,
//!   "oldest_full_content_at": "2026-04-04T01:23:45Z",
//!   "retention_horizon_at": "2026-04-04T01:23:45Z",
//!   "retention_days": 30,
//!   "per_layer_max_bytes_adk_provided": 65536,
//!   "per_layer_max_bytes_user_derived": 16384,
//!   "enabled": true,
//!   "restart_required_for_config_changes": true,
//!   "config_applied_at": "boot",
//!   "config_source": "agentdesk.yaml boot snapshot",
//!   "hot_reload": false
//! }
//! ```

use axum::{Json, extract::State, http::StatusCode};
use serde_json::json;

use super::AppState;

/// GET /api/prompt-manifest/retention
pub async fn get_retention_status(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };
    let config = state.config.prompt_manifest_retention.clone();

    match crate::db::prompt_manifests::manifest_storage_stats(pool, &config).await {
        Ok(stats) => (
            StatusCode::OK,
            Json(serde_json::to_value(stats).unwrap_or_else(|_| json!({}))),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("manifest_storage_stats failed: {error}")})),
        ),
    }
}
