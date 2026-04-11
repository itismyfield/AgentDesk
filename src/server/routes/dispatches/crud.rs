use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use crate::server::routes::AppState;

// ── Query / Body types ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListDispatchesQuery {
    pub status: Option<String>,
    pub kanban_card_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateDispatchBody {
    pub kanban_card_id: String,
    pub to_agent_id: String,
    pub dispatch_type: Option<String>,
    pub title: String,
    pub context: Option<serde_json::Value>,
    pub skip_outbox: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDispatchBody {
    pub status: Option<String>,
    pub result: Option<serde_json::Value>,
}

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/dispatches
pub async fn list_dispatches(
    State(state): State<AppState>,
    Query(params): Query<ListDispatchesQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state
        .dispatch_service()
        .list_dispatches(params.status.as_deref(), params.kanban_card_id.as_deref())
    {
        Ok(dispatches) => (StatusCode::OK, Json(json!({"dispatches": dispatches}))),
        Err(error) => error.into_json_response(),
    }
}

/// GET /api/dispatches/:id
pub async fn get_dispatch(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.dispatch_service().get_dispatch(&id) {
        Ok(dispatch) => (StatusCode::OK, Json(json!({"dispatch": dispatch}))),
        Err(error) => error.into_json_response(),
    }
}

/// POST /api/dispatches
pub async fn create_dispatch(
    State(state): State<AppState>,
    Json(body): Json<CreateDispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let input = crate::services::dispatches::CreateDispatchInput {
        kanban_card_id: body.kanban_card_id,
        to_agent_id: body.to_agent_id,
        dispatch_type: body.dispatch_type,
        title: body.title,
        context: body.context,
        skip_outbox: body.skip_outbox,
    };

    match state.dispatch_service().create_dispatch(input) {
        Ok(result) => (result.status, Json(json!({"dispatch": result.dispatch}))),
        Err(error) => error.into_json_response(),
    }
}

/// PATCH /api/dispatches/:id
pub async fn update_dispatch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let input = crate::services::dispatches::UpdateDispatchInput {
        status: body.status,
        result: body.result,
    };

    match state.dispatch_service().update_dispatch(&id, input) {
        Ok(dispatch) => (StatusCode::OK, Json(json!({"dispatch": dispatch}))),
        Err(error) => error.into_json_response(),
    }
}
