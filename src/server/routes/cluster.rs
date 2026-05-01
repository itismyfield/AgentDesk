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

#[derive(Debug, Deserialize)]
pub struct ResourceLocksQuery {
    #[serde(default)]
    pub include_expired: bool,
}

pub async fn list_resource_locks(
    State(state): State<AppState>,
    Query(params): Query<ResourceLocksQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::resource_locks::list_resource_locks(pool, params.include_expired).await {
        Ok(locks) => (
            StatusCode::OK,
            Json(json!({
                "locks": locks,
                "default_ttl_secs": crate::server::resource_locks::default_resource_lock_ttl_secs()
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}

pub async fn acquire_resource_lock(
    State(state): State<AppState>,
    Json(body): Json<crate::server::resource_locks::ResourceLockRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::resource_locks::acquire_resource_lock(pool, &body).await {
        Ok(outcome) => {
            let status = if outcome.acquired {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            };
            (status, Json(json!(outcome)))
        }
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))),
    }
}

pub async fn heartbeat_resource_lock(
    State(state): State<AppState>,
    Json(body): Json<crate::server::resource_locks::ResourceLockRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::resource_locks::heartbeat_resource_lock(pool, &body).await {
        Ok(Some(lock)) => (StatusCode::OK, Json(json!({"ok": true, "lock": lock}))),
        Ok(None) => (
            StatusCode::CONFLICT,
            Json(json!({"ok": false, "error": "lock is not held by requester or has expired"})),
        ),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))),
    }
}

#[derive(Debug, Deserialize)]
pub struct ResourceLockReleaseRequest {
    pub lock_key: String,
    pub holder_instance_id: String,
    pub holder_job_id: String,
}

pub async fn release_resource_lock(
    State(state): State<AppState>,
    Json(body): Json<ResourceLockReleaseRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::resource_locks::release_resource_lock(
        pool,
        &body.lock_key,
        &body.holder_instance_id,
        &body.holder_job_id,
    )
    .await
    {
        Ok(released) => (StatusCode::OK, Json(json!({"released": released}))),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))),
    }
}

pub async fn reclaim_expired_resource_locks(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::resource_locks::reclaim_expired_resource_locks(pool).await {
        Ok(reclaimed) => (StatusCode::OK, Json(json!({"reclaimed": reclaimed}))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error})),
        ),
    }
}
