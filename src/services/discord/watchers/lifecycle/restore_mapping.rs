use super::*;
use crate::services::tmux_common::tmux_owner_path;

pub(in crate::services::discord) fn session_belongs_to_current_runtime(
    session_name: &str,
    current_owner_marker: &str,
) -> bool {
    std::fs::read_to_string(tmux_owner_path(session_name))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| value == current_owner_marker)
        .unwrap_or(false)
}

pub(super) fn add_configured_channel_bindings(
    agent_sessions: &[&str],
    provider: &ProviderKind,
    configured_bindings: impl IntoIterator<
        Item = super::super::super::settings::RegisteredChannelBinding,
    >,
    name_to_channel: &mut std::collections::HashMap<String, (ChannelId, String)>,
) {
    for binding in configured_bindings {
        if binding.owner_provider != *provider {
            continue;
        }
        let Some(channel_name) = binding
            .fallback_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let tmux_name = provider.build_tmux_session_name(channel_name);
        if agent_sessions.iter().any(|session| **session == tmux_name) {
            name_to_channel
                .entry(tmux_name)
                .or_insert_with(|| (ChannelId::new(binding.channel_id), channel_name.to_string()));
        }
    }
}
