use axum::{Json, extract::State, http::StatusCode};
use serde_json::json;

use super::AppState;

/// GET /api/maintenance/jobs
pub async fn list_jobs(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let jobs = match state.pg_pool_ref() {
        Some(pool) => crate::server::maintenance::list_job_statuses_pg(pool.clone()).await,
        None => crate::server::maintenance::list_job_statuses_sqlite(state.db.clone()).await,
    };

    (StatusCode::OK, Json(json!({ "jobs": jobs })))
}
