use super::super::*;

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

pub(super) fn restored_intervention_message_ids(item: &Intervention) -> Vec<u64> {
    let mut item_ids: Vec<u64> = item.source_message_ids.iter().map(|id| id.get()).collect();
    if item_ids.is_empty() {
        item_ids.push(item.message_id.get());
    } else if !item_ids.contains(&item.message_id.get()) {
        item_ids.push(item.message_id.get());
    }
    item_ids
}

pub(super) fn enqueue_restored_intervention(
    existing_ids: &mut std::collections::HashSet<u64>,
    queue: &mut Vec<Intervention>,
    item: Intervention,
) -> bool {
    let item_ids = restored_intervention_message_ids(&item);
    // Persisted merged queue items may represent multiple source messages. If startup
    // catch-up already recovered only some of them, dropping the whole item would lose
    // the unseen messages because the merged text is no longer separable.
    if item_ids
        .iter()
        .all(|message_id| existing_ids.contains(message_id))
    {
        return false;
    }

    existing_ids.extend(item_ids);
    queue.push(item);
    true
}
