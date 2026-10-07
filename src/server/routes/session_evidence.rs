//! Session identity evidence without stale cleanup, liveness probes, or events.

use super::AppState;
use crate::{
    db::session_evidence::SessionEvidence,
    error::{AppError, AppResult, ErrorCode},
};
use axum::{
    Json,
    extract::{Path, State},
};

pub(crate) async fn get(
    State(state): State<AppState>,
    Path(identifier): Path<String>,
) -> AppResult<Json<SessionEvidence>> {
    let pool = state
        .pg_pool_ref()
        .ok_or_else(|| AppError::internal("postgres required"))?;
    let mut evidence = crate::db::session_evidence::load_session_evidence_pg(pool, &identifier)
        .await
        .map_err(|error| {
            AppError::internal(format!("query: {error}")).with_code(ErrorCode::Database)
        })?;
    match evidence.len() {
        0 => Err(AppError::not_found("session evidence binding not found")),
        1 => Ok(Json(evidence.remove(0))),
        _ => Err(AppError::conflict("session evidence binding is ambiguous")),
    }
}

#[cfg(all(test, unix))]
#[path = "session_evidence_tests.rs"]
mod tests;
