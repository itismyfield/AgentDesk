//! Provider and channel resolution shared by the agent turn entry routes.

use axum::{Json, http::StatusCode};
use serde_json::json;

use crate::services::provider::ProviderKind;

pub(super) struct AgentTurnTarget {
    pub(super) provider: ProviderKind,
    pub(super) primary_channel: String,
    pub(super) channel_id: u64,
}

/// Resolves the provider and channel an agent turn runs on, honoring the allowed overrides.
pub(super) async fn resolve_agent_turn_target(
    pool: &sqlx::PgPool,
    id: &str,
    provider_override: Option<&str>,
    channel_override: Option<&str>,
) -> Result<AgentTurnTarget, (StatusCode, Json<serde_json::Value>)> {
    let (provider, primary_channel) = {
        match crate::services::agents::query::agent_exists_pg(pool, id).await {
            Ok(true) => {}
            Ok(false) => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(json!({"ok": false, "error": "agent not found"})),
                ));
            }
            Err(error) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok": false, "error": format!("query: {error}")})),
                ));
            }
        }

        let Some(bindings) = crate::db::agents::load_agent_channel_bindings_pg(pool, id)
            .await
            .map_err(|error| error.to_string())
            .ok()
            .flatten()
        else {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"ok": false, "error": "agent channel binding not found"})),
            ));
        };

        if let Some(channel_override) = channel_override
            && !super::agents::channel_override_is_allowed(channel_override, &bindings)
        {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({
                    "ok": false,
                    "error": format!(
                        "channel override {} is not allowed for agent {}",
                        channel_override,
                        id
                    ),
                })),
            ));
        }

        let provider = match provider_override {
            Some(raw) => match ProviderKind::from_str(raw) {
                Some(kind) => kind,
                None => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "ok": false,
                            "error": format!("unsupported provider override: {raw}"),
                        })),
                    ));
                }
            },
            None => {
                let Some(kind) = bindings.resolved_primary_provider_kind() else {
                    return Err((
                        StatusCode::CONFLICT,
                        Json(
                            json!({"ok": false, "error": "agent primary provider is not configured"}),
                        ),
                    ));
                };
                kind
            }
        };

        let primary_channel = if let Some(chan) = channel_override.map(str::to_string) {
            chan
        } else if provider_override.is_some() {
            let Some(chan) = bindings.channel_for_provider(provider_override) else {
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({
                        "ok": false,
                        "error": format!(
                            "agent has no channel bound for provider {}",
                            provider_override.unwrap_or("")
                        ),
                    })),
                ));
            };
            chan
        } else {
            let Some(chan) = bindings.primary_channel() else {
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({"ok": false, "error": "agent primary channel is not configured"})),
                ));
            };
            chan
        };

        (provider, primary_channel)
    };

    let Some(channel_id_num) = super::dispatches::resolve_channel_alias_pub(&primary_channel)
        .or_else(|| primary_channel.parse::<u64>().ok())
    else {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "ok": false,
                "error": format!("agent primary channel is invalid: {}", primary_channel),
            })),
        ));
    };
    Ok(AgentTurnTarget {
        provider,
        primary_channel,
        channel_id: channel_id_num,
    })
}
