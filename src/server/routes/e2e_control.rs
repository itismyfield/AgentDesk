use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use poise::serenity_prelude::{ChannelId, MessageId};
use serde::Deserialize;
use serde_json::Value;

use super::AppState;
use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::discord::e2e_control::{self, DiscordFailureOperation};
use crate::services::provider::ProviderKind;

#[derive(Debug, Deserialize)]
pub(crate) struct DiscordE2eControlRequest {
    provider: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DiscordFailureInjectionRequest {
    provider: String,
    channel_id: String,
    operation: String,
    #[serde(default = "one")]
    count: u32,
}

fn one() -> u32 {
    1
}

fn require_enabled() -> AppResult<()> {
    if e2e_control::enabled() {
        Ok(())
    } else {
        Err(AppError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "E2E Discord controls are disabled",
        ))
    }
}

fn parse_provider(raw: &str) -> AppResult<ProviderKind> {
    ProviderKind::from_str(raw)
        .filter(ProviderKind::is_supported)
        .ok_or_else(|| AppError::bad_request("invalid provider"))
}

fn parse_snowflake<T>(raw: &str, label: &str, constructor: impl FnOnce(u64) -> T) -> AppResult<T> {
    let id = raw
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| AppError::bad_request(format!("invalid {label}")))?;
    Ok(constructor(id))
}

pub(crate) async fn delete_discord_message(
    State(state): State<AppState>,
    Path((channel_id_raw, message_id_raw)): Path<(String, String)>,
    Json(request): Json<DiscordE2eControlRequest>,
) -> AppResult<(StatusCode, Json<Value>)> {
    require_enabled()?;
    let provider = parse_provider(&request.provider)?;
    let channel_id = parse_snowflake(&channel_id_raw, "channel id", ChannelId::new)?;
    let message_id = parse_snowflake(&message_id_raw, "message id", MessageId::new)?;
    let registry = state.health_registry.as_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "Discord not available (standalone mode)",
        )
    })?;

    e2e_control::delete_message(registry, &provider, channel_id, message_id)
        .await
        .map_err(|error| {
            AppError::new(
                StatusCode::BAD_GATEWAY,
                ErrorCode::Discord,
                format!("Discord message delete failed: {error}"),
            )
        })?;
    Ok((
        StatusCode::OK,
        Json(Value::Object(serde_json::Map::from_iter([
            ("ok".to_string(), Value::Bool(true)),
            (
                "provider".to_string(),
                Value::String(provider.as_str().to_string()),
            ),
            (
                "channel_id".to_string(),
                Value::String(channel_id.get().to_string()),
            ),
            (
                "message_id".to_string(),
                Value::String(message_id.get().to_string()),
            ),
        ]))),
    ))
}

pub(crate) async fn inject_discord_failure(
    Json(request): Json<DiscordFailureInjectionRequest>,
) -> AppResult<(StatusCode, Json<Value>)> {
    require_enabled()?;
    let provider = parse_provider(&request.provider)?;
    let channel_id = parse_snowflake(&request.channel_id, "channel id", |id| id)?;
    let operation = DiscordFailureOperation::parse(&request.operation)
        .ok_or_else(|| AppError::bad_request("operation must be send or delete"))?;
    e2e_control::arm_failure(provider.clone(), channel_id, operation, request.count)
        .map_err(AppError::bad_request)?;
    Ok((
        StatusCode::OK,
        Json(Value::Object(serde_json::Map::from_iter([
            ("ok".to_string(), Value::Bool(true)),
            (
                "provider".to_string(),
                Value::String(provider.as_str().to_string()),
            ),
            (
                "channel_id".to_string(),
                Value::String(channel_id.to_string()),
            ),
            (
                "operation".to_string(),
                serde_json::to_value(operation).expect("serializable operation"),
            ),
            (
                "count".to_string(),
                Value::Number(serde_json::Number::from(request.count)),
            ),
        ]))),
    ))
}

pub(crate) async fn clear_discord_failure(
    Json(request): Json<DiscordFailureInjectionRequest>,
) -> AppResult<(StatusCode, Json<Value>)> {
    require_enabled()?;
    let provider = parse_provider(&request.provider)?;
    let channel_id = parse_snowflake(&request.channel_id, "channel id", |id| id)?;
    let operation = DiscordFailureOperation::parse(&request.operation)
        .ok_or_else(|| AppError::bad_request("operation must be send or delete"))?;
    let removed = e2e_control::clear_failure(&provider, channel_id, operation);
    Ok((
        StatusCode::OK,
        Json(Value::Object(serde_json::Map::from_iter([
            ("ok".to_string(), Value::Bool(true)),
            ("removed".to_string(), Value::Bool(removed)),
            (
                "provider".to_string(),
                Value::String(provider.as_str().to_string()),
            ),
            (
                "channel_id".to_string(),
                Value::String(channel_id.to_string()),
            ),
            (
                "operation".to_string(),
                serde_json::to_value(operation).expect("serializable operation"),
            ),
        ]))),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers_reject_unknown_provider_operation_and_zero_ids() {
        assert!(parse_provider("claude").is_ok());
        assert!(parse_provider("unknown-provider").is_err());
        assert_eq!(
            DiscordFailureOperation::parse("delete"),
            Some(DiscordFailureOperation::Delete)
        );
        assert_eq!(DiscordFailureOperation::parse("edit"), None);
        assert!(parse_snowflake("0", "message id", MessageId::new).is_err());
        assert!(parse_snowflake("abc", "message id", MessageId::new).is_err());
    }
}
