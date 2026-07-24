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

fn require_allowed_channel(channel_id: u64) -> AppResult<()> {
    if e2e_control::channel_is_allowed(channel_id) {
        Ok(())
    } else {
        Err(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Discord,
            "channel is not authorized for destructive E2E control",
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
    let provider = parse_provider(&request.provider)?;
    let channel_id = parse_snowflake(&channel_id_raw, "channel id", ChannelId::new)?;
    require_allowed_channel(channel_id.get())?;
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
    let provider = parse_provider(&request.provider)?;
    let channel_id = parse_snowflake(&request.channel_id, "channel id", |id| id)?;
    require_allowed_channel(channel_id)?;
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
    let provider = parse_provider(&request.provider)?;
    let channel_id = parse_snowflake(&request.channel_id, "channel id", |id| id)?;
    require_allowed_channel(channel_id)?;
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
    use axum::{
        body::Body,
        http::{Method, Request},
    };
    use tower::ServiceExt;

    fn test_api_router() -> axum::Router {
        let mut config = crate::config::Config::default();
        config.policies.hot_reload = false;
        let engine = crate::engine::PolicyEngine::new(&config).unwrap();
        let tx = crate::server::ws::new_broadcast();
        let buffer = crate::server::ws::spawn_batch_flusher(tx.clone());
        crate::server::routes::api_router_with_pg(engine, config, tx, buffer, None, None)
    }

    fn request(method: Method, uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

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

    #[tokio::test]
    async fn disabled_surface_is_uniformly_unmounted_for_methods_bodies_and_subpaths() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _control = crate::services::discord::e2e_control::TestControlGuard::set(false, [44]);
        let app = test_api_router();
        let cases = [
            (Method::GET, "/e2e/discord/failures", ""),
            (Method::POST, "/e2e/discord/failures", ""),
            (Method::POST, "/e2e/discord/failures", "not-json"),
            (Method::DELETE, "/e2e/discord/failures", "{}"),
            (Method::PUT, "/e2e/discord/failures", "{}"),
            (Method::DELETE, "/e2e/discord/channels/44/messages/55", ""),
            (
                Method::DELETE,
                "/e2e/discord/channels/44/messages/55",
                "not-json",
            ),
            (Method::GET, "/e2e/discord/channels/44/messages/55", ""),
            (Method::POST, "/e2e/discord/unknown/subpath", "{}"),
        ];
        for (method, uri, body) in cases {
            let response = app
                .clone()
                .oneshot(request(method.clone(), uri, body))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "disabled E2E surface leaked for {method} {uri}"
            );
        }
    }

    #[tokio::test]
    async fn enabled_surface_rejects_channels_outside_boot_allowlist() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _control = crate::services::discord::e2e_control::TestControlGuard::set(true, [44]);
        let app = test_api_router();

        let injection = app
            .clone()
            .oneshot(request(
                Method::POST,
                "/e2e/discord/failures",
                r#"{"provider":"claude","channel_id":"45","operation":"send"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(injection.status(), StatusCode::FORBIDDEN);

        let deletion = app
            .oneshot(request(
                Method::DELETE,
                "/e2e/discord/channels/45/messages/55",
                r#"{"provider":"claude"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(deletion.status(), StatusCode::FORBIDDEN);
    }
}
