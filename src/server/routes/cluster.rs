use axum::{Json, extract::State, http::StatusCode};
use serde_json::json;

use super::AppState;

pub async fn list_nodes(state: State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    let lease_ttl_secs = state.config.cluster.lease_ttl_secs.max(1);
    match crate::server::cluster::list_worker_nodes(pool, lease_ttl_secs).await {
        Ok(nodes) => (
            StatusCode::OK,
            Json(json!({
                "cluster": {
                    "enabled": state.config.cluster.enabled,
                    "configured_role": state.config.cluster.role,
                    "lease_ttl_secs": lease_ttl_secs,
                    "heartbeat_interval_secs": state.config.cluster.heartbeat_interval_secs.max(1),
                },
                "nodes": nodes,
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}
