use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::Value;
use serenity::model::id::ChannelId;

use super::AppState;
use crate::{
    error::{AppError, AppResult},
    services::{discord::turn_lease, provider::ProviderKind},
};

pub(super) async fn inspect(
    State(state): State<AppState>,
    Path((provider, channel_id)): Path<(String, std::num::NonZeroU64)>,
) -> AppResult<Json<Value>> {
    let provider = ProviderKind::from_str(&provider)
        .ok_or_else(|| AppError::bad_request("unknown provider"))?;
    let registry = state
        .health_registry
        .as_ref()
        .ok_or_else(|| AppError::internal("runtime unavailable"))?;
    let identity = turn_lease::inspect(registry, &provider, ChannelId::new(channel_id.get()))
        .await
        .map_err(AppError::conflict)?;
    Ok(Json(
        serde_json::to_value(identity).map_err(|e| AppError::internal(e.to_string()))?,
    ))
}

pub(super) async fn release(
    State(state): State<AppState>,
    Json(request): Json<turn_lease::ReleaseRequest>,
) -> AppResult<(StatusCode, Json<Value>)> {
    if request.expected.channel_id == 0 {
        return Err(AppError::bad_request("invalid channel_id"));
    }
    let registry = state
        .health_registry
        .as_ref()
        .ok_or_else(|| AppError::internal("runtime unavailable"))?;
    let value = turn_lease::release(registry, request)
        .await
        .map_err(AppError::conflict)?;
    Ok((StatusCode::OK, Json(value)))
}
