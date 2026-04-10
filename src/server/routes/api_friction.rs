use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct ApiFrictionQuery {
    pub limit: Option<usize>,
    pub min_events: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ProcessApiFrictionBody {
    pub min_events: Option<usize>,
    pub limit: Option<usize>,
}

pub async fn list_events(
    State(state): State<AppState>,
    Query(query): Query<ApiFrictionQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    match crate::services::api_friction::list_recent_api_friction_events(&state.db, query.limit) {
        Ok(events) => (StatusCode::OK, Json(json!({ "events": events }))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

pub async fn list_patterns(
    State(state): State<AppState>,
    Query(query): Query<ApiFrictionQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    match crate::services::api_friction::list_api_friction_patterns(
        &state.db,
        query.min_events,
        query.limit,
    ) {
        Ok(patterns) => (StatusCode::OK, Json(json!({ "patterns": patterns }))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

pub async fn process_patterns(
    State(state): State<AppState>,
    Json(body): Json<ProcessApiFrictionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    match crate::services::api_friction::process_api_friction_patterns(
        &state.db,
        body.min_events,
        body.limit,
    )
    .await
    {
        Ok(summary) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "summary": summary })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}
