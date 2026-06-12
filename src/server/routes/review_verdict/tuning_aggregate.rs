use axum::{Json, extract::State, http::StatusCode};

use super::super::AppState;

/// POST /api/reviews/tuning/aggregate
///
/// Thin route wrapper kept so inventory generation can resolve the route
/// handler source in the server route tree while the implementation lives in
/// `services::review_decision`.
pub async fn aggregate_review_tuning(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    crate::services::review_decision::aggregate_review_tuning(State(state)).await
}
