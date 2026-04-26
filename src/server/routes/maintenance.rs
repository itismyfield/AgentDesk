use axum::{Json, extract::State, http::StatusCode};
use serde_json::json;

use super::AppState;

/// GET /api/maintenance/jobs
pub async fn list_jobs(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };
    let jobs = crate::server::maintenance::list_job_statuses_pg(pool.clone()).await;

    (StatusCode::OK, Json(json!({ "jobs": jobs })))
}
