use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};

use super::AppState;
use crate::error::AppResult;

pub(crate) use crate::services::dispatched_sessions::force_kill_session_impl_with_reason;
pub use crate::services::dispatched_sessions::{
    DeleteSessionQuery, ForceKillOptions, HookSessionBody, KillTmuxOptions,
    ListDispatchedSessionsQuery, TmuxOutputQuery, UpdateDispatchedSessionBody,
};

/// GET /api/dispatched-sessions
pub async fn list_dispatched_sessions(
    State(state): State<AppState>,
    Query(params): Query<ListDispatchedSessionsQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    crate::services::dispatched_sessions::list_dispatched_sessions(State(state), Query(params))
        .await
}

/// POST /api/dispatched-sessions/webhook — upsert session from dcserver
pub async fn hook_session(
    State(state): State<AppState>,
    Json(body): Json<HookSessionBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    crate::services::dispatched_sessions::hook_session(State(state), Json(body)).await
}

/// DELETE /api/dispatched-sessions/cleanup — manual: delete disconnected sessions
pub async fn cleanup_sessions(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    crate::services::dispatched_sessions::cleanup_sessions(State(state)).await
}

/// DELETE /api/dispatched-sessions/gc-threads — periodic: delete stale thread sessions
pub async fn gc_thread_sessions(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    crate::services::dispatched_sessions::gc_thread_sessions(State(state)).await
}

/// DELETE /api/dispatched-sessions/webhook — delete a session by session_key
pub async fn delete_session(
    State(state): State<AppState>,
    Query(params): Query<DeleteSessionQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    crate::services::dispatched_sessions::delete_session(State(state), Query(params)).await
}

/// GET /api/dispatched-sessions/claude-session-id?session_key=...
pub async fn get_claude_session_id(
    State(state): State<AppState>,
    Query(params): Query<DeleteSessionQuery>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    crate::services::dispatched_sessions::get_claude_session_id(State(state), Query(params)).await
}

/// POST /api/dispatched-sessions/clear-stale-session-id
pub async fn clear_stale_session_id(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    crate::services::dispatched_sessions::clear_stale_session_id(State(state), Json(body)).await
}

/// POST /api/dispatched-sessions/clear-session-id
pub async fn clear_session_id_by_key(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    crate::services::dispatched_sessions::clear_session_id_by_key(State(state), Json(body)).await
}

/// PATCH /api/dispatched-sessions/:id
pub async fn update_dispatched_session(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateDispatchedSessionBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    crate::services::dispatched_sessions::update_dispatched_session(
        State(state),
        Path(id),
        Json(body),
    )
    .await
}

/// GET /api/sessions/{id}/tmux-output?lines=N
pub async fn tmux_output(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(params): Query<TmuxOutputQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    crate::services::dispatched_sessions::tmux_output(
        State(state),
        headers,
        Path(id),
        Query(params),
    )
    .await
}

/// POST /api/sessions/{session_key}/force-kill
pub async fn force_kill_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_key): Path<String>,
    Json(body): Json<ForceKillOptions>,
) -> (StatusCode, Json<serde_json::Value>) {
    crate::services::dispatched_sessions::force_kill_session(
        State(state),
        headers,
        Path(session_key),
        Json(body),
    )
    .await
}

/// POST /api/sessions/{session_key}/kill-tmux
pub async fn kill_tmux_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_key): Path<String>,
    Json(body): Json<KillTmuxOptions>,
) -> (StatusCode, Json<serde_json::Value>) {
    crate::services::dispatched_sessions::kill_tmux_session(
        State(state),
        headers,
        Path(session_key),
        Json(body),
    )
    .await
}
