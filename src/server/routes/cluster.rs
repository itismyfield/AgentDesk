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
        Ok(mut nodes) => {
            let (session_owners, session_owner_error) =
                match crate::db::dispatched_sessions::list_dispatched_sessions_pg(pool, false).await
                {
                    Ok(mut sessions) => {
                        crate::server::cluster_session_routing::enrich_session_owner_routing(
                            &mut sessions,
                            state.cluster_instance_id.as_deref(),
                            &nodes,
                        );
                        crate::server::cluster_session_routing::attach_active_session_counts_to_worker_nodes(
                            &mut nodes,
                            &sessions,
                        );
                        (
                            crate::server::cluster_session_routing::summarize_session_owner_routing(
                                &sessions,
                            ),
                            None,
                        )
                    }
                    Err(error) => {
                        tracing::warn!("failed to load active session owner summary: {error}");
                        crate::server::cluster_session_routing::attach_active_session_counts_to_worker_nodes(
                            &mut nodes,
                            &[],
                        );
                        (
                            json!({"total_active_sessions": null}),
                            Some(json!({
                                "code": "session_owner_summary_unavailable",
                                "message": "active session owner summary unavailable",
                            })),
                        )
                    }
                };
            (
                StatusCode::OK,
                Json(json!({
                    "cluster": {
                        "enabled": state.config.cluster.enabled,
                        "configured_role": state.config.cluster.role,
                        "lease_ttl_secs": lease_ttl_secs,
                        "heartbeat_interval_secs": state.config.cluster.heartbeat_interval_secs.max(1),
                        "local_worker_runtime": crate::server::worker_registry::leader_only_worker_status_json(),
                    },
                    "nodes": nodes,
                    "session_owners": session_owners,
                    "session_owner_error": session_owner_error,
                })),
            )
        }
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
            let routing_engine =
                crate::services::dispatches::routing_constraint::RoutingEngine::from_cluster_config(
                    &state.config.cluster,
                );
            let routing_dispatch =
                crate::services::dispatches::routing_constraint::RoutingDispatch::new(
                    "diagnostics",
                    None,
                    Some(required.clone()),
                );
            let routing = routing_engine.route(&nodes, &required, &routing_dispatch);
            let constraint_results = routing.constraint_results_json();
            let decisions = nodes
                .iter()
                .map(|node| crate::server::cluster::explain_capability_match(node, &required))
                .collect::<Vec<_>>();
            (
                StatusCode::OK,
                Json(json!({
                    "required": required,
                    "decisions": decisions,
                    "routing": routing,
                    "constraint_results": constraint_results,
                })),
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

pub async fn list_test_phase_runs(
    State(state): State<AppState>,
    Query(params): Query<crate::server::test_phase_runs::TestPhaseRunListQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::test_phase_runs::list_test_phase_runs(pool, &params).await {
        Ok(runs) => (StatusCode::OK, Json(json!({"runs": runs}))),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))),
    }
}

pub async fn upsert_test_phase_run(
    State(state): State<AppState>,
    Json(body): Json<crate::server::test_phase_runs::TestPhaseRunRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::test_phase_runs::upsert_test_phase_run(pool, &body).await {
        Ok(run) => (StatusCode::OK, Json(json!({"run": run}))),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))),
    }
}

pub async fn start_test_phase_run(
    State(state): State<AppState>,
    Json(body): Json<crate::server::test_phase_runs::TestPhaseRunStartRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::test_phase_runs::start_test_phase_run(pool, &body).await {
        Ok(outcome) => {
            let status = if outcome.started {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            };
            (status, Json(json!(outcome)))
        }
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))),
    }
}

pub async fn complete_test_phase_run(
    State(state): State<AppState>,
    Json(body): Json<crate::server::test_phase_runs::TestPhaseRunCompleteRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::test_phase_runs::complete_test_phase_run(pool, &body).await {
        Ok(outcome) => (StatusCode::OK, Json(json!(outcome))),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))),
    }
}

pub async fn latest_test_phase_evidence(
    State(state): State<AppState>,
    Query(params): Query<crate::server::test_phase_runs::TestPhaseEvidenceQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::test_phase_runs::latest_passing_evidence(
        pool,
        &params.phase_key,
        &params.head_sha,
    )
    .await
    {
        Ok(Some(run)) => (StatusCode::OK, Json(json!({"ok": true, "run": run}))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "passing evidence not found"})),
        ),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))),
    }
}

pub async fn claim_task_dispatches(
    State(state): State<AppState>,
    Json(body): Json<crate::server::task_dispatch_claims::TaskDispatchClaimRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::task_dispatch_claims::claim_task_dispatches(pool, &body).await {
        Ok(outcome) => (StatusCode::OK, Json(json!(outcome))),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))),
    }
}

pub async fn list_issue_specs(
    State(state): State<AppState>,
    Query(params): Query<crate::server::issue_specs::IssueSpecListQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::issue_specs::list_issue_specs(pool, &params).await {
        Ok(specs) => (StatusCode::OK, Json(json!({"specs": specs}))),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))),
    }
}

pub async fn upsert_issue_spec(
    State(state): State<AppState>,
    Json(body): Json<crate::server::issue_specs::IssueSpecUpsertRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres unavailable"})),
        );
    };
    match crate::server::issue_specs::upsert_issue_spec(pool, &body).await {
        Ok(spec) => (StatusCode::OK, Json(json!({"spec": spec}))),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))),
    }
}
