use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Deserialize;
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

#[derive(Debug, Deserialize)]
pub struct RoutingDiagnosticsQuery {
    pub required: Option<String>,
}

pub async fn routing_diagnostics(
    State(state): State<AppState>,
    Query(params): Query<RoutingDiagnosticsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    let required = match params.required.as_deref() {
        Some(raw) if !raw.trim().is_empty() => {
            match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(value) => value,
                Err(error) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": format!("invalid required JSON: {error}")})),
                    );
                }
            }
        }
        _ => json!({}),
    };
    let lease_ttl_secs = state.config.cluster.lease_ttl_secs.max(1);
    match crate::server::cluster::list_worker_nodes(pool, lease_ttl_secs).await {
        Ok(nodes) => {
            let decisions = nodes
                .iter()
                .map(|node| crate::server::cluster::explain_capability_match(node, &required))
                .collect::<Vec<_>>();
            (
                StatusCode::OK,
                Json(json!({ "required": required, "decisions": decisions })),
            )
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}
