use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::session_canonical_identity::HookCanonicalIdentity;
use super::{RoleBinding, SharedData};
use crate::services::provider::ProviderKind;

pub(super) async fn post_status(
    session_key: &str,
    name: Option<&str>,
    model: Option<&str>,
    status: &str,
    provider: &ProviderKind,
    session_info: Option<&str>,
    tokens: Option<u64>,
    cwd: Option<&str>,
    dispatch_id: Option<&str>,
    thread_channel_id: Option<u64>,
    channel_id: Option<serenity::ChannelId>,
    agent_id: Option<&str>,
    canonical: Option<HookCanonicalIdentity<'_>>,
) {
    let status = crate::db::session_status::normalize_incoming_session_status(Some(status));
    let body = crate::services::dispatched_sessions::HookSessionBody {
        session_key: session_key.to_string(),
        instance_id: None,
        agent_id: agent_id.map(str::to_string),
        status: Some(status.to_string()),
        provider: Some(provider.as_str().to_string()),
        session_info: session_info.map(str::to_string),
        name: clean_nonempty(name).map(str::to_string),
        model: clean_nonempty(model)
            .filter(|value| !value.eq_ignore_ascii_case(provider.as_str()))
            .map(str::to_string),
        tokens,
        cwd: clean_nonempty(cwd).map(str::to_string),
        dispatch_id: clean_nonempty(dispatch_id).map(str::to_string),
        thread_channel_id: thread_channel_id.map(|id| id.to_string()),
        claude_session_id: None,
        session_id: None,
        channel_id: channel_id.map(|id| id.get().to_string()),
        identity_kind: canonical.map(|identity| identity.identity_kind.to_string()),
        discord_token_hash: canonical.map(|identity| identity.discord_token_hash.to_string()),
        turn_start_nonce: None,
        dispatched_origin: None,
    };

    if let Err(err) = super::internal_api::hook_session(body).await {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ ADK session POST failed: {err}");
    }
}

fn clean_nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

pub(super) async fn post_canonical(
    session_key: Option<&str>,
    name: Option<&str>,
    model: Option<&str>,
    status: &str,
    provider: &ProviderKind,
    session_info: Option<&str>,
    tokens: Option<u64>,
    cwd: Option<&str>,
    dispatch_id: Option<&str>,
    thread_channel_id: Option<u64>,
    channel_id: Option<serenity::ChannelId>,
    agent_id: Option<&str>,
    token_hash: &str,
    scheduled_snapshot: bool,
    _api_port: u16,
) {
    let Some(session_key) = session_key else {
        return;
    };
    let canonical = super::session_canonical_identity::identity_for_session_key(
        session_key,
        provider,
        token_hash,
        scheduled_snapshot,
    );
    post_status(
        session_key,
        name,
        model,
        status,
        provider,
        session_info,
        tokens,
        cwd,
        dispatch_id,
        thread_channel_id,
        channel_id,
        agent_id,
        canonical,
    )
    .await;
}

pub(super) async fn post_channel_turn(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    session_key: Option<&str>,
    name: Option<&str>,
    model: Option<&str>,
    provider: &ProviderKind,
    session_info: &str,
    cwd: &str,
    dispatch_id: Option<&str>,
    thread_channel_id: Option<u64>,
    role_binding: Option<&RoleBinding>,
) {
    post_canonical(
        session_key,
        name,
        model,
        "working",
        provider,
        Some(session_info),
        None,
        Some(cwd),
        dispatch_id,
        thread_channel_id,
        Some(channel_id),
        role_binding.map(|binding| binding.role_id.as_str()),
        &shared.token_hash,
        false,
        shared.api_port,
    )
    .await;
}

pub(super) async fn post_legacy(
    session_key: Option<&str>,
    name: Option<&str>,
    model: Option<&str>,
    status: &str,
    provider: &ProviderKind,
    session_info: Option<&str>,
    tokens: Option<u64>,
    cwd: Option<&str>,
    dispatch_id: Option<&str>,
    thread_channel_id: Option<u64>,
    channel_id: Option<serenity::ChannelId>,
    agent_id: Option<&str>,
    _api_port: u16,
) {
    let Some(session_key) = session_key else {
        return;
    };
    post_status(
        session_key,
        name,
        model,
        status,
        provider,
        session_info,
        tokens,
        cwd,
        dispatch_id,
        thread_channel_id,
        channel_id,
        agent_id,
        None,
    )
    .await;
}
