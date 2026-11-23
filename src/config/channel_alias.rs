//! Channel alias lookup over the agents' configured channel bindings.

pub(crate) fn resolve_channel_alias(alias: &str) -> Option<u64> {
    let alias = alias.trim();
    if alias.is_empty() {
        return None;
    }

    let config = super::runtime_config_optional()?;
    for agent in &config.agents {
        for (_provider_key, channel) in agent.channels.iter() {
            if !channel.aliases().iter().any(|candidate| candidate == alias) {
                continue;
            }
            if let Some(id) = channel
                .channel_id()
                .and_then(|value| value.parse::<u64>().ok())
            {
                return Some(id);
            }
        }
    }
    None
}
