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

    resolve_bound_target(&bindings, id, provider_override, channel_override)
}

fn target_error(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(json!({"ok": false, "error": message.into()})))
}

fn resolve_bound_target(
    bindings: &crate::db::agents::AgentChannelBindings,
    id: &str,
    provider_override: Option<&str>,
    channel_override: Option<&str>,
) -> Result<AgentTurnTarget, (StatusCode, Json<serde_json::Value>)> {
    if let Some(channel) = channel_override
        && !super::agents::channel_override_is_allowed(channel, bindings)
    {
        return Err(target_error(
            StatusCode::FORBIDDEN,
            format!("channel override {channel} is not allowed for agent {id}"),
        ));
    }
    let requested = provider_override
        .map(|raw| {
            ProviderKind::from_str(raw).ok_or_else(|| {
                target_error(
                    StatusCode::BAD_REQUEST,
                    format!("unsupported provider override: {raw}"),
                )
            })
        })
        .transpose()?;
    if requested.is_none()
        && channel_override.is_none()
        && bindings.resolved_primary_provider_kind().is_none()
    {
        return Err(target_error(
            StatusCode::CONFLICT,
            "agent primary provider is not configured",
        ));
    }
    let primary_channel = channel_override
        .map(str::to_owned)
        .or_else(|| {
            if provider_override.is_some() {
                bindings.channel_for_provider(provider_override)
            } else {
                bindings.primary_channel()
            }
        })
        .ok_or_else(|| {
            target_error(
                StatusCode::CONFLICT,
                "agent channel is not configured for the requested provider",
            )
        })?;
    let provider = bindings
        .provider_for_channel(|channel| {
            super::agents::channel_identifier_matches(channel, &primary_channel)
        })
        .ok_or_else(|| {
            target_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "channel provider is missing or ambiguous",
            )
        })?;
    if requested.is_some_and(|requested| requested != provider) {
        return Err(target_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "provider override does not match channel binding",
        ));
    }
    let channel_id = super::dispatches::resolve_channel_alias_pub(&primary_channel)
        .or_else(|| primary_channel.parse::<u64>().ok())
        .ok_or_else(|| {
            target_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("agent primary channel is invalid: {primary_channel}"),
            )
        })?;
    Ok(AgentTurnTarget {
        provider,
        primary_channel,
        channel_id,
    })
}

/// 409 when a turn that never claimed the mailbox (TUI-direct, adopted or monitor) still
/// holds the channel, or a turn-mode transcript reads busy or unknown: a start there would
/// report `started` and then lose the prompt. A retired runtime answers 422 before any hold.
pub(super) async fn external_turn_conflict(
    registry: Option<&crate::services::discord::health::HealthRegistry>,
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<(StatusCode, Json<serde_json::Value>)> {
    use crate::services::discord::health::{ExternalHold, external_turn_hold_for_start};
    if let Some(refused) = retired_target(provider, channel_id) {
        return Some(refused);
    }
    let hold = external_turn_hold_for_start(registry, provider, channel_id).await?;
    let error = match hold {
        ExternalHold::Unknown => format!("channel {channel_id}'s TUI turn state is unknown"),
        _ => format!("an external TUI turn holds channel {channel_id}"),
    };
    let reason = hold.reason();
    let body = json!({"ok": false, "status": "conflict", "reason": reason, "error": error});
    Some((StatusCode::CONFLICT, Json(body)))
}

fn retired_target(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<(StatusCode, Json<serde_json::Value>)> {
    use crate::services::provider_hosting::{RetiredRuntimeError, ensure_runtime_executable};
    ensure_runtime_executable(provider, Some(channel_id)).err()?;
    let (reason, error) = (
        RetiredRuntimeError::CODE,
        RetiredRuntimeError::NEW_INPUT_NOTICE,
    );
    let body = json!({"ok": false, "status": "invalid_target", "reason": reason, "error": error});
    Some((StatusCode::UNPROCESSABLE_ENTITY, Json(body)))
}

/// The supervised presence of the agent's turn channel; null outside turn mode.
pub(super) async fn turn_presence(pool: &sqlx::PgPool, id: &str) -> serde_json::Value {
    let target = resolve_agent_turn_target(pool, id, None, None).await.ok();
    let status = crate::services::discord::health::turn_presence_status;
    target
        .and_then(|target| status(target.channel_id))
        .unwrap_or(serde_json::Value::Null)
}

/// Starts a headless turn on a resolved agent target; returns its turn id and start status.
/// Shared by the turn-start route and the voice conductor.
pub(super) async fn start_headless_turn_on_target(
    registry: &crate::services::discord::health::HealthRegistry,
    target: AgentTurnTarget,
    prompt: String,
    source: Option<String>,
    metadata: Option<serde_json::Value>,
) -> Result<(String, &'static str), crate::services::discord::HeadlessTurnStartError> {
    use crate::services::provider_hosting::{RetiredRuntimeError, ensure_runtime_executable};
    if ensure_runtime_executable(&target.provider, Some(target.channel_id)).is_err() {
        let reason = RetiredRuntimeError::CODE.to_string();
        return Err(crate::services::discord::HeadlessTurnStartError::InvalidTarget(reason));
    }
    let channel_name_hint = (!target.primary_channel.chars().all(|ch| ch.is_ascii_digit()))
        .then_some(target.primary_channel);
    crate::services::discord::health::start_headless_agent_turn(
        registry,
        poise::serenity_prelude::ChannelId::new(target.channel_id),
        target.provider,
        prompt,
        source,
        metadata,
        channel_name_hint,
    )
    .await
    .map(|outcome| (outcome.turn_id, outcome.status.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_turn_channel_override_selects_its_provider_and_rejects_mismatch() {
        let bindings = crate::db::agents::AgentChannelBindings {
            provider: Some("codex".into()),
            discord_channel_id: Some("101".into()),
            discord_channel_cc: Some("101".into()),
            discord_channel_cdx: Some("102".into()),
            discord_channel_alt: Some("103".into()),
        };
        for (channel, explicit, expected) in [
            (None, None, ProviderKind::Codex),
            (Some("101"), None, ProviderKind::Claude),
            (Some("00101"), None, ProviderKind::Claude),
            (Some("102"), None, ProviderKind::Codex),
            (Some("103"), None, ProviderKind::Codex),
            (Some("101"), Some("claude"), ProviderKind::Claude),
            (None, Some("claude"), ProviderKind::Claude),
        ] {
            let target = resolve_bound_target(&bindings, "dual", explicit, channel)
                .ok()
                .unwrap();
            assert_eq!(
                target.provider, expected,
                "channel={channel:?} explicit={explicit:?}"
            );
            if let Some(channel) = channel {
                assert_eq!(target.channel_id, channel.parse::<u64>().unwrap());
            }
        }
        let err = resolve_bound_target(&bindings, "dual", Some("codex"), Some("101"))
            .err()
            .unwrap();
        assert_eq!(err.0, StatusCode::UNPROCESSABLE_ENTITY);
        let err = resolve_bound_target(&bindings, "dual", None, Some("999"))
            .err()
            .unwrap();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        let empty = crate::db::agents::AgentChannelBindings::default();
        let err = resolve_bound_target(&empty, "unbound", None, None)
            .err()
            .unwrap();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(err.1.0["error"], "agent primary provider is not configured");
        let mut ambiguous = bindings.clone();
        ambiguous.discord_channel_cdx = Some("101".into());
        assert_eq!(
            resolve_bound_target(&ambiguous, "dual", None, Some("101"))
                .err()
                .unwrap()
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        for provider in ["claude", "codex", "gemini"] {
            let single = crate::db::agents::AgentChannelBindings {
                provider: Some(provider.into()),
                discord_channel_id: Some("201".into()),
                ..Default::default()
            };
            let target = resolve_bound_target(&single, "single", None, Some("201"))
                .ok()
                .unwrap();
            assert_eq!(target.provider.as_str(), provider);
        }
    }

    /// `/turn/start` and the voice conductor refuse a retired runtime, held or not, once resolved
    /// and before the external hold or a start; otherwise the held channel answers its 409.
    #[tokio::test(flavor = "current_thread")]
    async fn a_retired_runtime_refuses_turn_start_before_the_hold_and_the_start() {
        use crate::services::discord::HeadlessTurnStartError;
        use crate::services::discord::health::{HealthRegistry, seed_external_turn_row_for_tests};
        use crate::services::provider_hosting::{RetiredRuntimeError, retired_runtime_for_tests};
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (channel, unheld) = (6_570_621_u64, 6_570_622_u64);
        let provider = ProviderKind::Claude;
        seed_external_turn_row_for_tests(&provider, channel);
        let registry = HealthRegistry::new();
        let conflict = |channel: u64| {
            let (registry, provider) = (&registry, &provider);
            async move {
                let response = external_turn_conflict(Some(registry), provider, channel).await;
                response.map(|(status, Json(body))| {
                    let reason = body["reason"].as_str().unwrap_or("-").to_string();
                    format!("{} {reason}", status.as_u16())
                })
            }
        };
        let start = || {
            let target = AgentTurnTarget {
                provider: provider.clone(),
                primary_channel: channel.to_string(),
                channel_id: channel,
            };
            start_headless_turn_on_target(&registry, target, "status?".to_string(), None, None)
        };
        let retired_start = |result: &Result<(String, &'static str), HeadlessTurnStartError>| {
            let code = RetiredRuntimeError::CODE;
            matches!(result, Err(HeadlessTurnStartError::InvalidTarget(reason)) if reason == code)
        };

        {
            let _retired = retired_runtime_for_tests::retire(&provider, Some(channel));
            let _unheld = retired_runtime_for_tests::retire(&provider, Some(unheld));
            for target in [channel, unheld] {
                let refused = conflict(target).await;
                assert_eq!(refused.as_deref(), Some("422 runtime_retired:claude-e"));
            }
            let started = start().await;
            assert!(retired_start(&started), "{started:?}");
        }
        let held = conflict(channel).await;
        assert_eq!(held.as_deref(), Some("409 external_turn_active"));
        assert_eq!(conflict(unheld).await, None);
        let started = start().await;
        assert!(!retired_start(&started), "{started:?}");
    }
}
