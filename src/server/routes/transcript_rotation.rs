use axum::{
    Json,
    extract::{FromRequest, Path, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use poise::serenity_prelude::ChannelId;
use serde_json::json;

use super::AppState;
use crate::db::dispatched_session_canonical_identity::{
    HookSessionUpsertError, resolve_session_key_pg,
};
use crate::domain::provider::ProviderKind;
use crate::error::AppError;
use crate::services::turn_orchestrator::{
    QueuePersistenceContext,
    queued_head::v2::rotation::{Handoff, HandoffError, Target},
};

pub(super) async fn submit(
    State(state): State<AppState>,
    Path(key): Path<String>,
    request: Request,
) -> Result<Response, AppError> {
    require_enabled(&state)?;
    let Json(handoff) = match Json::<Handoff>::from_request(request, &state).await {
        Ok(handoff) => handoff,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    if handoff.body.trim().is_empty() {
        return Err(AppError::bad_request("rotation handoff body is empty"));
    }
    let receipt = target(&state, &key)
        .await?
        .stage(handoff)
        .await
        .map_err(handoff_error)?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(receipt)).into_response())
}

pub(super) async fn status(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Response, AppError> {
    require_enabled(&state)?;
    let rotation = target(&state, &key)
        .await?
        .status()
        .await
        .map_err(handoff_error)?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "rotation": rotation })),
    )
        .into_response())
}

fn require_enabled(state: &AppState) -> Result<(), AppError> {
    let _ = state;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Session {
    id: i64,
    channel_id: Option<String>,
    discord_token_hash: Option<String>,
    instance_id: Option<String>,
}

async fn target(state: &AppState, key: &str) -> Result<Target, AppError> {
    let unavailable = || handoff_error(HandoffError::Unavailable);
    let pool = state.pg_pool.as_ref().ok_or_else(unavailable)?;
    let key = resolve_session_key_pg(pool, key)
        .await
        .map_err(|error| match error {
            HookSessionUpsertError::Conflict(_) => handoff_error(HandoffError::Conflict),
            HookSessionUpsertError::Database(_) => unavailable(),
        })?
        .ok_or_else(|| AppError::not_found("session not found"))?;
    let session: Session = sqlx::query_as(
        "SELECT id, channel_id, discord_token_hash, instance_id FROM sessions
         WHERE session_key = $1 AND provider = 'claude'",
    )
    .bind(key)
    .fetch_optional(pool)
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(|| AppError::not_found("Claude session not found"))?;
    if session.instance_id.is_some()
        && session.instance_id.as_deref() != state.cluster_instance_id.as_deref()
    {
        return Err(handoff_error(HandoffError::Conflict));
    }
    let channel_id = session
        .channel_id
        .and_then(|id| id.parse::<u64>().ok())
        .filter(|id| *id != 0)
        .ok_or_else(|| handoff_error(HandoffError::Conflict))?;
    let token_hash = session
        .discord_token_hash
        .filter(|hash| {
            !hash.is_empty()
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
        .ok_or_else(|| handoff_error(HandoffError::Conflict))?;
    Ok(Target {
        session_row_id: session.id,
        channel: ChannelId::new(channel_id),
        persistence: QueuePersistenceContext::new(&ProviderKind::Claude, &token_hash, None),
    })
}

fn handoff_error(error: HandoffError) -> AppError {
    match error {
        HandoffError::Conflict => AppError::conflict("rotation handoff identity or body differs"),
        HandoffError::Gone => AppError::new(
            StatusCode::GONE,
            crate::error::ErrorCode::NotFound,
            "rotation ticket closed",
        ),
        HandoffError::Unavailable => AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            crate::error::ErrorCode::Queue,
            "rotation owner or session store unavailable",
        ),
    }
}

#[cfg(test)]
mod rotation_tests;
