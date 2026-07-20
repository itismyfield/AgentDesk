//! Axum route handlers for dispatch thread linking and lookup.
//!
//! Thread-reuse persistence and lookup decisions live in
//! `crate::services::dispatches::thread_reuse`; these handlers keep the HTTP
//! boundary focused on extraction, validation, and response conversion.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde_json::{Value, json};

use crate::error::{AppError, AppResult};
use crate::server::routes::AppState;
use crate::services::dispatches::LinkDispatchThreadBody;
use crate::services::dispatches::thread_reuse::{
    DispatchThreadReuseError, LinkDispatchThreadInput, get_card_thread_pg,
    get_pending_dispatch_for_thread_pg, link_dispatch_thread_pg,
};

fn postgres_pool_unavailable() -> AppError {
    AppError::internal("postgres pool unavailable")
}

fn thread_reuse_error_response(error: DispatchThreadReuseError) -> AppError {
    match error {
        DispatchThreadReuseError::NotFound(message) => AppError::not_found(message),
        DispatchThreadReuseError::Internal(message) => AppError::internal(message),
    }
}

// ── Route handlers ────────────────────────────────────────────
/// POST /api/internal/link-dispatch-thread
/// Links a dispatch's kanban card to a Discord thread (sets active_thread_id).
/// Called by dcserver router.rs when it creates a thread as fallback.
pub async fn link_dispatch_thread(
    State(state): State<AppState>,
    Json(body): Json<LinkDispatchThreadBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let pool = state.pg_pool_ref().ok_or_else(postgres_pool_unavailable)?;

    let input = LinkDispatchThreadInput {
        dispatch_id: body.dispatch_id,
        thread_id: body.thread_id,
        channel_id: body.channel_id,
    };
    match link_dispatch_thread_pg(pool, input).await {
        Ok(outcome) => Ok((
            StatusCode::OK,
            Json(json!({"ok": true, "card_id": outcome.card_id})),
        )),
        Err(error) => Err(thread_reuse_error_response(error)),
    }
}

/// GET /api/internal/card-thread?dispatch_id=xxx
/// Returns the active_thread_id for a dispatch's card (if any).
pub async fn get_card_thread(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let dispatch_id = match params.get("dispatch_id") {
        Some(id) => id,
        None => return Err(AppError::bad_request("dispatch_id required")),
    };

    let pool = state.pg_pool_ref().ok_or_else(postgres_pool_unavailable)?;

    match get_card_thread_pg(pool, dispatch_id).await {
        Ok(outcome) => Ok((
            StatusCode::OK,
            Json(json!({
                "card_id": outcome.card_id,
                "card_title": outcome.card_title,
                "github_issue_url": outcome.github_issue_url,
                "github_issue_number": outcome.github_issue_number,
                "issue_body": outcome.issue_body,
                "deferred_dod": outcome.deferred_dod,
                "active_thread_id": outcome.active_thread_id,
                "dispatch_type": outcome.dispatch_type,
                "dispatch_title": outcome.dispatch_title,
                "discord_channel_id": outcome.discord_channel_id,
                "discord_channel_alt": outcome.discord_channel_alt,
                "discord_channel_target": outcome.discord_channel_target,
                "dispatch_context": outcome.dispatch_context,
            })),
        )),
        Err(error) => Err(thread_reuse_error_response(error)),
    }
}

/// GET /api/internal/pending-dispatch-for-thread?thread_id=xxx
///
/// #222: Look up the latest pending/dispatched dispatch whose kanban card is
/// linked to the given thread channel. Used by turn_bridge as fallback when
/// parse_dispatch_id(user_text) fails in unified/reused threads, including
/// review and review-decision flows.
pub async fn get_pending_dispatch_for_thread(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let thread_id = match params.get("thread_id") {
        Some(id) => id,
        None => return Err(AppError::bad_request("thread_id required")),
    };

    let pool = state.pg_pool_ref().ok_or_else(postgres_pool_unavailable)?;

    match get_pending_dispatch_for_thread_pg(pool, thread_id).await {
        Ok(Some(id)) => Ok((StatusCode::OK, Json(json!({"dispatch_id": id})))),
        Ok(None) => Err(AppError::not_found("no pending dispatch for thread")),
        Err(error) => Err(thread_reuse_error_response(error)),
    }
}
