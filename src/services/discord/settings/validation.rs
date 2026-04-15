use super::*;

pub(crate) fn channel_supports_provider(
    provider: &ProviderKind,
    channel_name: Option<&str>,
    is_dm: bool,
    role_binding: Option<&RoleBinding>,
) -> bool {
    if is_dm {
        return provider.is_supported();
    }

    if let Some(bound_provider) = role_binding.and_then(|binding| binding.provider.as_ref()) {
        return bound_provider == provider;
    }

    if let Some(ch) = channel_name {
        if let Some(mapped) = lookup_suffix_provider(ch) {
            return mapped == *provider;
        }
    }

    if org_schema::org_schema_exists() {
        return false;
    }

    provider.is_channel_supported(
        channel_name,
        is_dm,
        role_binding.and_then(|binding| binding.provider.as_ref()),
    )
}

pub(crate) fn bot_settings_allow_channel(
    settings: &DiscordBotSettings,
    channel_id: ChannelId,
    is_dm: bool,
) -> bool {
    if is_dm {
        return true;
    }
    settings.allowed_channel_ids.is_empty()
        || settings.allowed_channel_ids.contains(&channel_id.get())
}

pub(crate) fn bot_settings_allow_agent(
    settings: &DiscordBotSettings,
    role_binding: Option<&RoleBinding>,
    is_dm: bool,
) -> bool {
    if is_dm {
        return true;
    }

    let Some(expected_agent) = settings
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return true;
    };

    role_binding.is_some_and(|binding| binding.role_id.eq_ignore_ascii_case(expected_agent))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BotChannelRoutingGuardFailure {
    ChannelNotAllowed,
    AgentMismatch,
    ProviderMismatch,
}

impl std::fmt::Display for BotChannelRoutingGuardFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChannelNotAllowed => f.write_str("not allowed for bot settings"),
            Self::AgentMismatch => f.write_str("agent mismatch"),
            Self::ProviderMismatch => f.write_str("provider mismatch"),
        }
    }
}

impl BotChannelRoutingGuardFailure {
    pub(crate) fn is_expected_cross_bot_skip(self) -> bool {
        matches!(self, Self::ChannelNotAllowed | Self::AgentMismatch)
    }
}

pub(crate) fn validate_bot_channel_routing(
    settings: &DiscordBotSettings,
    provider: &ProviderKind,
    channel_id: ChannelId,
    channel_name: Option<&str>,
    is_dm: bool,
) -> Result<(), BotChannelRoutingGuardFailure> {
    validate_bot_channel_routing_with_provider_channel(
        settings,
        provider,
        channel_id,
        channel_name,
        channel_name,
        is_dm,
    )
}

pub(crate) fn validate_bot_channel_routing_with_provider_channel(
    settings: &DiscordBotSettings,
    provider: &ProviderKind,
    allowlist_channel_id: ChannelId,
    binding_channel_name: Option<&str>,
    provider_channel_name: Option<&str>,
    is_dm: bool,
) -> Result<(), BotChannelRoutingGuardFailure> {
    let role_binding = resolve_role_binding(
        allowlist_channel_id,
        binding_channel_name.or(provider_channel_name),
    );

    if !bot_settings_allow_channel(settings, allowlist_channel_id, is_dm) {
        return Err(BotChannelRoutingGuardFailure::ChannelNotAllowed);
    }
    if !bot_settings_allow_agent(settings, role_binding.as_ref(), is_dm) {
        return Err(BotChannelRoutingGuardFailure::AgentMismatch);
    }
    if !channel_supports_provider(
        provider,
        provider_channel_name.or(binding_channel_name),
        is_dm,
        role_binding.as_ref(),
    ) {
        return Err(BotChannelRoutingGuardFailure::ProviderMismatch);
    }

    Ok(())
}

fn lookup_suffix_provider(channel_name: &str) -> Option<ProviderKind> {
    if org_schema::org_schema_exists() {
        if let Some(provider) = org_schema::lookup_suffix_provider(channel_name) {
            return Some(provider);
        }
    }
    let path = bot_settings_path()?;
    let content = fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let map = json.get("suffix_map")?.as_object()?;
    for (suffix, provider_val) in map {
        if channel_name.ends_with(suffix.as_str()) {
            let provider_str = provider_val.as_str()?;
            return Some(ProviderKind::from_str_or_unsupported(provider_str));
        }
    }
    None
}

pub(crate) fn resolve_role_binding(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<RoleBinding> {
    if let Some(binding) = agentdesk_config::resolve_role_binding(channel_id, channel_name) {
        return Some(binding);
    }
    if org_schema::org_schema_exists() {
        if let Some(binding) = org_schema::resolve_role_binding(channel_id, channel_name) {
            return Some(binding);
        }
    }
    resolve_role_binding_from_role_map(channel_id, channel_name)
}

pub(crate) fn list_registered_channel_bindings() -> Vec<RegisteredChannelBinding> {
    let mut merged = std::collections::BTreeMap::<u64, RegisteredChannelBinding>::new();

    for binding in list_registered_channel_bindings_from_role_map() {
        merged.insert(binding.channel_id, binding);
    }

    if org_schema::org_schema_exists() {
        for binding in org_schema::list_registered_channel_bindings() {
            merged.insert(binding.channel_id, binding);
        }
    }

    for binding in agentdesk_config::list_registered_channel_bindings() {
        merged.insert(binding.channel_id, binding);
    }

    merged.into_values().collect()
}

pub(crate) fn resolve_workspace(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<String> {
    if let Some(ws) = agentdesk_config::resolve_workspace(channel_id, channel_name) {
        return Some(ws);
    }
    if org_schema::org_schema_exists() {
        if let Some(ws) = org_schema::resolve_workspace(channel_id, channel_name) {
            return Some(ws);
        }
    }
    resolve_workspace_from_role_map(channel_id, channel_name)
}

pub(crate) fn has_configured_channel_binding(
    channel_id: ChannelId,
    _channel_name: Option<&str>,
) -> bool {
    resolve_role_binding(channel_id, None).is_some()
        || resolve_workspace(channel_id, None).is_some()
}
