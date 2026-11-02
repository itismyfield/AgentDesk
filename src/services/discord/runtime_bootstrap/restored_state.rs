use super::super::*;

/// The channel model overrides saved in `bot_settings`, as boot restores them.
pub(in crate::services::discord) fn restored_model_overrides(
    bot_settings: &DiscordBotSettings,
) -> Vec<(ChannelId, String)> {
    bot_settings
        .channel_model_overrides
        .iter()
        .filter_map(|(channel_id, model)| {
            channel_id
                .parse::<u64>()
                .ok()
                .map(|id| (ChannelId::new(id), model.clone()))
        })
        .collect()
}

/// Puts restored model overrides into the map a turn reads its model from.
pub(in crate::services::discord) fn restore_model_overrides(
    into: &dashmap::DashMap<ChannelId, String>,
    restored: &[(ChannelId, String)],
) {
    for (channel_id, model) in restored {
        into.insert(*channel_id, model.clone());
    }
}

pub(super) fn restored_fast_mode_enabled_channels_for_provider(
    bot_settings: &DiscordBotSettings,
    _provider: &ProviderKind,
) -> Vec<ChannelId> {
    let mut channels: Vec<ChannelId> = bot_settings
        .channel_fast_modes
        .iter()
        .filter_map(|(channel_id, enabled)| {
            if !*enabled {
                return None;
            }
            channel_id.parse::<u64>().ok().map(ChannelId::new)
        })
        .collect();
    channels.sort_unstable_by_key(|channel_id| channel_id.get());
    channels
}

pub(super) fn restored_fast_mode_reset_entries(bot_settings: &DiscordBotSettings) -> Vec<String> {
    let mut entries: Vec<String> = bot_settings
        .channel_fast_mode_reset_pending
        .iter()
        .cloned()
        .collect();
    entries.sort_unstable();
    entries
}

pub(super) fn restored_fast_mode_reset_channels(
    bot_settings: &DiscordBotSettings,
) -> Vec<ChannelId> {
    let mut channels: Vec<ChannelId> = bot_settings
        .channel_fast_mode_reset_pending
        .iter()
        .filter_map(|entry| {
            let raw_channel_id = entry
                .split_once(':')
                .map(|(_, channel_id)| channel_id)
                .unwrap_or(entry.as_str());
            raw_channel_id.parse::<u64>().ok().map(ChannelId::new)
        })
        .collect();
    channels.sort_unstable_by_key(|channel_id| channel_id.get());
    channels.dedup_by_key(|channel_id| channel_id.get());
    channels
}

pub(super) fn restored_codex_goals_enabled_channels(
    bot_settings: &DiscordBotSettings,
) -> Vec<ChannelId> {
    let mut channels: Vec<ChannelId> = bot_settings
        .channel_codex_goals
        .iter()
        .filter_map(|(channel_id, enabled)| {
            if !*enabled {
                return None;
            }
            channel_id.parse::<u64>().ok().map(ChannelId::new)
        })
        .collect();
    channels.sort_unstable_by_key(|channel_id| channel_id.get());
    channels
}

pub(super) fn restored_codex_goals_reset_channels(
    bot_settings: &DiscordBotSettings,
) -> Vec<ChannelId> {
    let mut channels: Vec<ChannelId> = bot_settings
        .channel_codex_goals_reset_pending
        .iter()
        .filter_map(|channel_id| channel_id.parse::<u64>().ok().map(ChannelId::new))
        .collect();
    channels.sort_unstable_by_key(|channel_id| channel_id.get());
    channels
}

pub(super) fn bootstrap_session_reset_pending_channels(
    restored_model_overrides: &[(ChannelId, String)],
    restored_fast_mode_reset_channels: &[ChannelId],
    restored_codex_goals_reset_channels: &[ChannelId],
) -> dashmap::DashSet<ChannelId> {
    let _ = restored_model_overrides;
    let set = dashmap::DashSet::new();
    for channel_id in restored_fast_mode_reset_channels {
        set.insert(*channel_id);
    }
    for channel_id in restored_codex_goals_reset_channels {
        set.insert(*channel_id);
    }
    set
}
