use super::*;
use std::io;

pub(in crate::services::discord) const EFFORT_LEVELS: [&str; 6] =
    ["low", "medium", "high", "xhigh", "max", "ultra"];

/// The channel's session name and the folder its next launch starts in.
pub(super) async fn session_facts(
    shared: &Arc<SharedData>,
    channel: serenity::ChannelId,
) -> (Option<String>, Option<std::path::PathBuf>) {
    let core = shared.core.lock().await;
    let session = core.sessions.get(&channel);
    (
        session.and_then(|session| session.channel_name.clone()),
        session.and_then(|session| session.current_path.as_deref().map(Into::into)),
    )
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
    (model, cwd): (Option<&str>, Option<&std::path::Path>),
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
        super::super::effort_capability::for_model(provider, model, channel, cwd)
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
        && !crate::config::session_hosts::has_herdr_endpoint(channel.get())
        && resolve_provider_session_selection_with_channel(provider, true, Some(channel.get()))
            .driver
            == ProviderSessionDriver::TuiHosting
}

fn not_local(provider: &ProviderKind) -> String {
    format!(
        "/effort는 로컬 {} TUI 채널에서만 저장할 수 있습니다.",
        provider.display_name()
    )
}

/// Reads what `/effort` takes on this channel, the one answer behind its suggestions and its
/// command. The read may run the provider CLI, so it stays off the async workers.
async fn read_entry_capability<T: Send + 'static>(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: serenity::ChannelId,
    read: impl FnOnce(&super::super::effort_capability::Capability) -> T + Send + 'static,
) -> Result<T, String> {
    use super::super::effort_capability::{for_model, herdr_claude, refused};
    use crate::services::provider_hosting::{
        ProviderSessionDriver, resolve_provider_session_selection_with_channel,
    };
    if let Err(error) = shared.check_settings_owner(provider, channel).await {
        return Ok(read(&refused(error.to_string())));
    }
    let (name, cwd) = session_facts(shared, channel).await;
    let dispatch_role = shared
        .dispatch
        .role_overrides
        .get(&channel)
        .map(|role| *role);
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
    let provider = provider.clone();
    tokio::task::spawn_blocking(move || {
        let known = matches!(provider, ProviderKind::Claude | ProviderKind::Codex);
        let capability = if known && crate::config::session_hosts::has_herdr_endpoint(channel.get())
        {
            let driver = resolve_provider_session_selection_with_channel(
                &provider,
                true,
                Some(channel.get()),
            )
            .driver;
            if matches!(provider, ProviderKind::Claude)
                && driver == ProviderSessionDriver::TuiHosting
            {
                herdr_claude()
            } else {
                refused("Herdr /effort는 live Claude TUI pane에서만 지원합니다.".into())
            }
        } else if known && !local_tui(&provider, channel) {
            refused(not_local(&provider))
        } else {
            for_model(&provider, model.as_deref(), channel, cwd.as_deref())
        };
        read(&capability)
    })
    .await
    .map_err(|error| format!("effort 지원 확인이 중단됐습니다: {error}"))
}

/// The levels `/effort` suggests for what has been typed so far.
pub(in crate::services::discord) async fn effort_entry_choices(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: serenity::ChannelId,
    typed: &str,
) -> Vec<String> {
    let prefix = typed.trim().to_lowercase();
    let read =
        move |capability: &super::super::effort_capability::Capability| capability.choices(&prefix);
    read_entry_capability(shared, provider, channel, read)
        .await
        .unwrap_or_default()
}

/// The canonical level for what `/effort` was given, if this channel takes it now.
pub(in crate::services::discord) async fn validate_effort_entry(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: serenity::ChannelId,
    given: &str,
) -> Result<&'static str, String> {
    // Text that is no level is never echoed back into the channel.
    let level = EFFORT_LEVELS
        .into_iter()
        .find(|level| *level == given.trim())
        .ok_or("알 수 없는 effort 값입니다. 자동완성 선택지에서 골라 주세요.")?;
    read_entry_capability(shared, provider, channel, move |capability| {
        capability.supports(level)
    })
    .await??;
    Ok(level)
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
        return Err(io::Error::other(not_local(provider)));
    }
    if !EFFORT_LEVELS.contains(&level) {
        return Err(io::Error::other(format!(
            "지원하지 않는 effort 값입니다: {level}"
        )));
    }
    let (name, cwd) = session_facts(shared, channel).await;
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
    super::super::effort_capability::for_model(provider, model.as_deref(), channel, cwd.as_deref())
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
