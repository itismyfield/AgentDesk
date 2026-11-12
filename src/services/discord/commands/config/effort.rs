use super::*;
use std::io;

pub(in crate::services::discord) const EFFORT_LEVELS: [&str; 6] =
    ["low", "medium", "high", "xhigh", "max", "ultra"];

pub(super) async fn channel_name(
    shared: &Arc<SharedData>,
    channel: serenity::ChannelId,
) -> Option<String> {
    shared
        .core
        .lock()
        .await
        .sessions
        .get(&channel)
        .and_then(|session| session.channel_name.clone())
}

pub(super) fn model_for_write(
    provider: &ProviderKind,
    channel: serenity::ChannelId,
    channel_name: Option<&str>,
    dispatch_role: Option<serenity::ChannelId>,
    explicit: Option<&str>,
) -> Option<String> {
    let dispatch = dispatch_role
        .and_then(|id| resolve_role_binding(id, None))
        .and_then(|binding| binding.model);
    let role = resolve_role_binding(channel, channel_name).and_then(|binding| binding.model);
    let (model, source) = resolve_effective_model(explicit, dispatch.as_deref(), role.as_deref());
    runtime_model_for_turn(provider, &model, source)
}

pub(super) fn check_saved_model(
    settings: &super::super::super::DiscordBotSettings,
    channel: serenity::ChannelId,
    provider: &ProviderKind,
    model: Option<&str>,
) -> io::Result<()> {
    if let Some(level) = settings
        .channel_effort_levels
        .get(&channel.to_string())
        .filter(|level| {
            level.as_str() == "ultra"
                || matches!(provider, ProviderKind::Codex)
                    && EFFORT_LEVELS.contains(&level.as_str())
        })
    {
        super::super::effort_capability::for_model(provider, model, channel)
            .supports(level)
            .map_err(|reason| {
                io::Error::other(format!(
                    "{level}가 저장된 채널의 모델을 바꾸지 않았습니다: {reason}"
                ))
            })?;
    }
    Ok(())
}

pub(in crate::services::discord) async fn channel_effort_level(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: serenity::ChannelId,
) -> Option<String> {
    if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
        return None;
    }
    shared
        .settings
        .read()
        .await
        .channel_effort_levels
        .get(&channel.to_string())
        .filter(|level| EFFORT_LEVELS.contains(&level.as_str()))
        .cloned()
}

fn local_tui(provider: &ProviderKind, channel: serenity::ChannelId) -> bool {
    use crate::services::provider_hosting::{
        ProviderSessionDriver, resolve_provider_session_selection_with_channel,
    };
    matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
        && crate::config::session_hosts::herdr_endpoint(channel.get()).is_none()
        && resolve_provider_session_selection_with_channel(provider, true, Some(channel.get()))
            .driver
            == ProviderSessionDriver::TuiHosting
}

pub(in crate::services::discord) async fn update_channel_effort(
    shared: &Arc<SharedData>,
    token: &str,
    channel: serenity::ChannelId,
    provider: &ProviderKind,
    level: &str,
) -> io::Result<bool> {
    shared.check_settings_owner(provider, channel).await?;
    if !local_tui(provider, channel) {
        return Err(io::Error::other(format!(
            "/effort는 로컬 {} TUI 채널에서만 저장할 수 있습니다.",
            provider.display_name()
        )));
    }
    if !EFFORT_LEVELS.contains(&level) {
        return Err(io::Error::other(format!(
            "지원하지 않는 effort 값입니다: {level}"
        )));
    }
    let name = channel_name(shared, channel).await;
    let mut settings = shared.settings.write().await;
    // Route publication uses this slot; keep it stable through the synchronous settings commit.
    let role = shared.dispatch.role_overrides.entry(channel);
    let dispatch_role = match &role {
        dashmap::mapref::entry::Entry::Occupied(entry) => Some(*entry.get()),
        dashmap::mapref::entry::Entry::Vacant(_) => None,
    };
    let explicit = shared
        .overrides
        .model_overrides
        .get(&channel)
        .map(|value| value.clone());
    let model = model_for_write(
        provider,
        channel,
        name.as_deref(),
        dispatch_role,
        explicit.as_deref(),
    );
    super::super::effort_capability::for_model(provider, model.as_deref(), channel)
        .supports(level)
        .map_err(io::Error::other)?;
    let key = channel.to_string();
    if settings.channel_effort_levels.get(&key).map(String::as_str) == Some(level) {
        return Ok(false);
    }
    let mut candidate = settings.clone();
    candidate
        .channel_effort_levels
        .insert(key.clone(), level.to_string());
    candidate.channel_relaunch_pending.insert(key);
    candidate.try_save_bot_settings(token)?;
    super::super::control::forget_relaunch_failures(channel);
    *settings = candidate;
    Ok(true)
}
