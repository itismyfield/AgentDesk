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
    provider: &ProviderKind,
    channel_id: ChannelId,
    is_dm: bool,
) -> bool {
    if is_dm {
        return true;
    }
    if settings.allowed_channel_ids.is_empty()
        || settings.allowed_channel_ids.contains(&channel_id.get())
    {
        return true;
    }
    // Voice channels are declared only via `agents[].voice.channel_id`, never in
    // a bot's `auth.allowed_channel_ids`, so a non-empty allowlist that lacks the
    // voice channel would otherwise block `/voice join` (#3902). Treat the
    // configured voice channel as allowed for its owning provider bot only —
    // mirroring the `resolve_role_binding` voice patch. Non-owning providers stay
    // blocked here and are caught again by the provider-match check.
    agentdesk_config::is_voice_channel_owned_by_provider(channel_id, provider)
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
    // Always resolve role binding against the same channel identity used for
    // allowlist checks (parent for threads). Do not allow live thread names to
    // influence agent binding resolution.
    let role_binding = resolve_role_binding(allowlist_channel_id, provider_channel_name);

    if !bot_settings_allow_channel(settings, provider, allowlist_channel_id, is_dm) {
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

/// Resolve the prompt-cache TTL bucket (#1088) for a Discord channel.
/// Currently only `agentdesk_config` channels expose this field; other
/// binding sources fall back to `None` (default 5m).
pub(crate) fn resolve_cache_ttl_minutes(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<u32> {
    agentdesk_config::resolve_cache_ttl_minutes(channel_id, channel_name)
}

pub(crate) fn resolve_dispatch_profile(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<super::super::DispatchProfile> {
    agentdesk_config::resolve_dispatch_profile(channel_id, channel_name)
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

#[cfg(test)]
mod voice_channel_guard_tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    // A voice channel declared only via `agents[].voice.channel_id`; it is never
    // present in any bot's `auth.allowed_channel_ids`.
    const VOICE_CHANNEL_ID: u64 = 1504612455916245163;
    // A normal text-channel binding for the owning (codex) agent.
    const TEXT_CHANNEL_ID: u64 = 1479671301387059200;
    // An unrelated channel that is neither in the allowlist nor a voice channel.
    const UNRELATED_CHANNEL_ID: u64 = 1111111111111111111;

    fn with_temp_root<F>(f: F)
    where
        F: FnOnce(),
    {
        // Serialize on the process-wide `AGENTDESK_ROOT_DIR` lock so this
        // root-mutating helper cannot race a concurrent test in another module.
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp = TempDir::new().expect("temp home");
        let root = temp.path().join(".adk");
        let settings_dir = root.join("config");
        fs::create_dir_all(&settings_dir).unwrap();
        fs::write(
            settings_dir.join("agentdesk.yaml"),
            r#"
server:
  port: 8791
agents:
  - id: project-agentdesk
    name: "AgentDesk"
    provider: codex
    voice:
      channel_id: "1504612455916245163"
    channels:
      codex:
        id: "1479671301387059200"
        name: "adk-cdx"
"#,
        )
        .unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        f();
        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    fn bot_settings(provider: ProviderKind, allowed_channel_ids: Vec<u64>) -> DiscordBotSettings {
        DiscordBotSettings {
            provider,
            allowed_channel_ids,
            agent: Some("project-agentdesk".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn allow_channel_recognizes_owner_voice_channel_without_allowlist_entry() {
        with_temp_root(|| {
            // codex owns the voice channel; its allowlist has only the text
            // channel, NOT the voice channel.
            let codex = bot_settings(ProviderKind::Codex, vec![TEXT_CHANNEL_ID]);

            assert!(
                bot_settings_allow_channel(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(VOICE_CHANNEL_ID),
                    false,
                ),
                "owning provider must be allowed in its configured voice channel",
            );

            // No allow-all regression: an unrelated channel that is neither in
            // the allowlist nor a voice channel stays blocked.
            assert!(
                !bot_settings_allow_channel(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(UNRELATED_CHANNEL_ID),
                    false,
                ),
                "non-allowlisted, non-voice channel must stay blocked",
            );

            // A non-owning provider with a non-empty allowlist that lacks the
            // voice channel is not granted the voice exception.
            let claude = bot_settings(ProviderKind::Claude, vec![TEXT_CHANNEL_ID]);
            assert!(
                !bot_settings_allow_channel(
                    &claude,
                    &ProviderKind::Claude,
                    ChannelId::new(VOICE_CHANNEL_ID),
                    false,
                ),
                "non-owning provider must not inherit the voice-channel exception",
            );
        });
    }

    #[test]
    fn full_guard_passes_voice_slash_command_for_owner_blocks_non_owner() {
        with_temp_root(|| {
            // Owner (codex) with a non-empty allowlist that omits the voice
            // channel — a slash command in the voice channel must pass the guard.
            let codex = bot_settings(ProviderKind::Codex, vec![TEXT_CHANNEL_ID]);
            assert!(
                validate_bot_channel_routing(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(VOICE_CHANNEL_ID),
                    None,
                    false,
                )
                .is_ok(),
                "owning provider's voice slash command must pass the command guard",
            );

            // No allow-all regression: an unrelated channel stays blocked for the
            // owner with ChannelNotAllowed.
            assert_eq!(
                validate_bot_channel_routing(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(UNRELATED_CHANNEL_ID),
                    None,
                    false,
                ),
                Err(BotChannelRoutingGuardFailure::ChannelNotAllowed),
                "unrelated channel must still be blocked",
            );

            // Non-owning provider (claude) with an empty allowlist (allow-all)
            // is still blocked by the provider-match check, mirroring the live
            // bug report.
            let claude = bot_settings(ProviderKind::Claude, Vec::new());
            assert_eq!(
                validate_bot_channel_routing(
                    &claude,
                    &ProviderKind::Claude,
                    ChannelId::new(VOICE_CHANNEL_ID),
                    None,
                    false,
                ),
                Err(BotChannelRoutingGuardFailure::ProviderMismatch),
                "non-owning provider must stay blocked in the voice channel",
            );
        });
    }
}
