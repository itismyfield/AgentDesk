use super::legacy_session_key_from_namespaced;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude as serenity;

/// Clear the stored provider session_id from DB for a given session_key.
/// Called when the user runs /clear so the next turn doesn't resume a dead session.
pub(in crate::services::discord) async fn clear_provider_session_id(
    session_key: &str,
    _api_port: u16,
) {
    if let Err(err) = super::super::internal_api::clear_session_id(session_key).await {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ clear_provider_session_id failed: {err}");
    }

    if let Some(legacy_key) = legacy_session_key_from_namespaced(session_key) {
        let _ = super::super::internal_api::clear_session_id(&legacy_key).await;
    }
}

/// Requires the existing durable clear route's acknowledgement, including legacy aliases.
pub(in crate::services::discord) async fn clear_provider_session_id_checked(
    session_key: &str,
) -> Result<(), String> {
    let response = super::super::internal_api::clear_session_id(session_key).await?;
    if response
        .get("cleared")
        .and_then(serde_json::Value::as_u64)
        .is_none()
    {
        return Err(format!(
            "invalid selector clear acknowledgement: {response}"
        ));
    }
    if let Some(legacy_key) = legacy_session_key_from_namespaced(session_key) {
        let response = super::super::internal_api::clear_session_id(&legacy_key).await?;
        if response
            .get("cleared")
            .and_then(serde_json::Value::as_u64)
            .is_none()
        {
            return Err(format!(
                "invalid legacy selector clear acknowledgement: {response}"
            ));
        }
    }
    Ok(())
}

/// Save a provider session selector to DB so it survives dcserver restarts.
/// The executable selector stays in the legacy `claude_session_id` column for
/// compatibility, while the raw observed provider session id travels through
/// `session_id` and is persisted separately by the server route.
pub(in crate::services::discord) async fn save_provider_session_id(
    session_key: &str,
    session_id: &str,
    raw_provider_session_id: Option<&str>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    _api_port: u16,
) {
    let body = provider_session_body(
        session_key,
        session_id,
        raw_provider_session_id,
        provider,
        channel_id,
    );
    if let Err(err) = super::super::internal_api::hook_session(body).await {
        tracing::warn!(error = %err, "save_provider_session_id failed");
    }
}

pub(in crate::services::discord) async fn save_provider_session_id_checked(
    session_key: &str,
    session_id: &str,
    raw_provider_session_id: Option<&str>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> Result<(), String> {
    let body = provider_session_body(
        session_key,
        session_id,
        raw_provider_session_id,
        provider,
        channel_id,
    );
    let response = super::super::internal_api::hook_session(body).await?;
    if response.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(format!("invalid selector save acknowledgement: {response}"));
    }
    Ok(())
}

fn provider_session_body(
    session_key: &str,
    session_id: &str,
    raw_provider_session_id: Option<&str>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> crate::services::dispatched_sessions::HookSessionBody {
    crate::services::dispatched_sessions::HookSessionBody {
        session_key: session_key.to_string(),
        instance_id: None,
        agent_id: None,
        status: None,
        provider: Some(provider.as_str().to_string()),
        session_info: None,
        name: None,
        model: None,
        tokens: None,
        cwd: None,
        dispatch_id: None,
        thread_channel_id: None,
        claude_session_id: Some(session_id.to_string()),
        session_id: raw_provider_session_id.map(str::to_string),
        channel_id: Some(channel_id.get().to_string()),
        identity_kind: None,
        discord_token_hash: None,
        turn_start_nonce: None,
        dispatched_origin: None,
    }
}

#[cfg(test)]
#[path = "adk_session_selector_checked_tests.rs"]
mod selector_checked_tests;
