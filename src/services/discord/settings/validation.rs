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

    /// #3869: does a restart-time routing-validation failure mean an in-flight
    /// inflight row is GENUINELY ORPHANED (clean up + notify) rather than merely
    /// re-routable to a sibling bot (preserve)?
    ///
    /// `ChannelNotAllowed` / `AgentMismatch` are `is_expected_cross_bot_skip`
    /// outcomes: another same-provider bot owns that channel/agent and runs its
    /// own recovery pass over the SAME persisted rows, so this bot must NOT touch
    /// the row (clearing it would destroy the owning bot's recoverable turn).
    /// `ProviderMismatch` means the channel was re-bound to a DIFFERENT provider
    /// entirely — no same-provider sibling can adopt this row and the new
    /// provider has no row for it, so it is genuinely orphaned and must be
    /// finalized instead of silently stranded for the placeholder sweeper.
    pub(crate) fn orphans_inflight_on_restart(self) -> bool {
        !self.is_expected_cross_bot_skip()
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

/// Validate a live Discord channel without letting a thread's parent erase an
/// authoritative direct child binding.
///
/// A directly-bound thread is its own routing scope.  Only an unbound thread
/// may fall back to its typed Discord parent, and that fallback must honor the
/// parent's `threadInherit` policy.  When inheritance is disabled we validate
/// the child identity, which predictably rejects bots that only own the parent
/// instead of accidentally accepting the wrong bot through that parent.
pub(crate) fn validate_bot_channel_routing_with_thread_parent(
    settings: &DiscordBotSettings,
    provider: &ProviderKind,
    channel_id: ChannelId,
    channel_name: Option<&str>,
    thread_parent: Option<(ChannelId, Option<&str>)>,
    is_dm: bool,
) -> Result<(), BotChannelRoutingGuardFailure> {
    if is_dm || resolve_role_binding(channel_id, channel_name).is_some() {
        return validate_bot_channel_routing_with_provider_channel(
            settings,
            provider,
            channel_id,
            channel_name,
            channel_name,
            is_dm,
        );
    }

    if let Some((parent_id, parent_name)) = thread_parent
        && thread_inheritance_enabled(parent_id, parent_name)
    {
        return validate_bot_channel_routing_with_provider_channel(
            settings,
            provider,
            parent_id,
            parent_name,
            parent_name,
            is_dm,
        );
    }

    validate_bot_channel_routing_with_provider_channel(
        settings,
        provider,
        channel_id,
        channel_name,
        channel_name,
        is_dm,
    )
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

fn resolve_thread_inherit(
    parent_channel_id: ChannelId,
    parent_channel_name: Option<&str>,
) -> Option<bool> {
    if let Some(enabled) =
        agentdesk_config::resolve_thread_inherit(parent_channel_id, parent_channel_name)
    {
        return Some(enabled);
    }
    if org_schema::org_schema_exists()
        && let Some(enabled) =
            org_schema::resolve_thread_inherit(parent_channel_id, parent_channel_name)
    {
        return Some(enabled);
    }
    resolve_thread_inherit_from_role_map(parent_channel_id, parent_channel_name)
}

pub(crate) fn thread_inheritance_enabled(
    parent_channel_id: ChannelId,
    parent_channel_name: Option<&str>,
) -> bool {
    resolve_thread_inherit(parent_channel_id, parent_channel_name).unwrap_or(true)
}

/// Resolve a Discord thread's role from its parent after a direct thread
/// lookup has failed. The caller owns Discord channel-kind detection and
/// supplies the already-resolved parent, keeping config resolution synchronous.
pub(crate) fn resolve_inherited_role_binding(
    parent_channel_id: ChannelId,
    parent_channel_name: Option<&str>,
) -> Option<RoleBinding> {
    if !thread_inheritance_enabled(parent_channel_id, parent_channel_name) {
        return None;
    }
    resolve_role_binding(parent_channel_id, parent_channel_name)
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

/// Resolve a Discord thread's workspace from its parent after a direct thread
/// lookup has failed, honoring the same `threadInherit` gate as role binding.
pub(crate) fn resolve_inherited_workspace(
    parent_channel_id: ChannelId,
    parent_channel_name: Option<&str>,
) -> Option<String> {
    if !thread_inheritance_enabled(parent_channel_id, parent_channel_name) {
        return None;
    }
    resolve_workspace(parent_channel_id, parent_channel_name)
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
    const THREAD_CHANNEL_ID: u64 = 1504612455916245999;

    const DEFAULT_TEST_CONFIG: &str = r#"
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
        prompt_file: "/tmp/project-agentdesk.md"
        workspace: "/tmp/agentdesk"
"#;

    fn with_temp_root_files<F>(
        agentdesk_yaml: Option<&str>,
        role_map_json: Option<&str>,
        org_yaml: Option<&str>,
        f: F,
    ) where
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
        if let Some(yaml) = agentdesk_yaml {
            fs::write(settings_dir.join("agentdesk.yaml"), yaml).unwrap();
        }
        if let Some(json) = role_map_json {
            fs::write(settings_dir.join("role_map.json"), json).unwrap();
        }
        if let Some(yaml) = org_yaml {
            fs::write(settings_dir.join("org.yaml"), yaml).unwrap();
        }
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        f();
        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    fn with_temp_root<F>(f: F)
    where
        F: FnOnce(),
    {
        with_temp_root_files(Some(DEFAULT_TEST_CONFIG), None, None, f);
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

    #[test]
    fn thread_inherits_parent_role_memory_and_workspace_by_default() {
        with_temp_root(|| {
            let parent = ChannelId::new(TEXT_CHANNEL_ID);
            let direct = resolve_role_binding(parent, Some("adk-cdx")).expect("direct role");
            let inherited =
                resolve_inherited_role_binding(parent, Some("adk-cdx")).expect("inherited role");

            assert_eq!(inherited.role_id, direct.role_id);
            assert_eq!(inherited.prompt_file, direct.prompt_file);
            assert_eq!(inherited.provider, direct.provider);
            assert_eq!(inherited.memory, direct.memory);
            assert_eq!(
                resolve_inherited_workspace(parent, Some("adk-cdx")),
                resolve_workspace(parent, Some("adk-cdx")),
            );
        });
    }

    #[test]
    fn thread_inherit_false_blocks_role_and_workspace_without_changing_parent_binding() {
        with_temp_root_files(
            Some(&DEFAULT_TEST_CONFIG.replace(
                "        workspace: \"/tmp/agentdesk\"",
                "        workspace: \"/tmp/agentdesk\"\n        threadInherit: false",
            )),
            None,
            None,
            || {
                let parent = ChannelId::new(TEXT_CHANNEL_ID);
                assert!(resolve_role_binding(parent, Some("adk-cdx")).is_some());
                assert!(resolve_workspace(parent, Some("adk-cdx")).is_some());
                assert!(resolve_inherited_role_binding(parent, Some("adk-cdx")).is_none());
                assert!(resolve_inherited_workspace(parent, Some("adk-cdx")).is_none());
            },
        );
    }

    #[test]
    fn legacy_role_map_thread_inherit_false_is_honored_and_unbound_stays_unbound() {
        let role_map = format!(
            r#"{{
  "byChannelId": {{
    "{TEXT_CHANNEL_ID}": {{
      "roleId": "project-agentdesk",
      "promptFile": "/tmp/project-agentdesk.md",
      "provider": "codex",
      "workspace": "/tmp/agentdesk",
      "threadInherit": false
    }}
  }}
}}"#
        );
        with_temp_root_files(None, Some(&role_map), None, || {
            let parent = ChannelId::new(TEXT_CHANNEL_ID);
            assert!(resolve_role_binding(parent, None).is_some());
            assert!(resolve_inherited_role_binding(parent, None).is_none());
            assert!(resolve_inherited_workspace(parent, None).is_none());

            let unbound_parent = ChannelId::new(UNRELATED_CHANNEL_ID);
            assert!(resolve_role_binding(unbound_parent, None).is_none());
            assert!(resolve_inherited_role_binding(unbound_parent, None).is_none());
            assert!(resolve_inherited_workspace(unbound_parent, None).is_none());
        });
    }

    #[test]
    fn org_schema_thread_inherit_false_blocks_parent_fallback() {
        let org = format!(
            r#"version: 1
agents:
  project-agentdesk:
    display_name: AgentDesk
    provider: codex
    workspace: /tmp/org-workspace
channels:
  by_id:
    "{TEXT_CHANNEL_ID}":
      agent: project-agentdesk
      threadInherit: false
"#,
        );
        with_temp_root_files(None, None, Some(&org), || {
            let parent = ChannelId::new(TEXT_CHANNEL_ID);
            assert!(resolve_role_binding(parent, None).is_some());
            assert_eq!(
                resolve_workspace(parent, None).as_deref(),
                Some("/tmp/org-workspace"),
            );
            assert!(resolve_inherited_role_binding(parent, None).is_none());
            assert!(resolve_inherited_workspace(parent, None).is_none());
        });
    }

    fn routing_role_map(thread_inherit: bool, include_child: bool) -> String {
        let child = if include_child {
            format!(
                r#",
    "{THREAD_CHANNEL_ID}": {{
      "roleId": "review-agent",
      "promptFile": "/tmp/review-agent.md",
      "provider": "claude"
    }}"#
            )
        } else {
            String::new()
        };
        format!(
            r#"{{
  "byChannelId": {{
    "{TEXT_CHANNEL_ID}": {{
      "roleId": "project-agentdesk",
      "promptFile": "/tmp/project-agentdesk.md",
      "provider": "codex",
      "threadInherit": {thread_inherit}
    }}{child}
  }}
}}"#
        )
    }

    #[test]
    fn live_thread_routing_prefers_direct_child_binding_over_parent() {
        let role_map = routing_role_map(true, true);
        with_temp_root_files(None, Some(&role_map), None, || {
            let parent = Some((ChannelId::new(TEXT_CHANNEL_ID), Some("adk-cdx")));
            let codex = bot_settings(ProviderKind::Codex, vec![TEXT_CHANNEL_ID]);
            assert_eq!(
                validate_bot_channel_routing_with_thread_parent(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    Some("review-thread"),
                    parent,
                    false,
                ),
                Err(BotChannelRoutingGuardFailure::ChannelNotAllowed),
                "the parent-owning bot must not accept a directly bound child"
            );

            let mut claude = bot_settings(ProviderKind::Claude, vec![THREAD_CHANNEL_ID]);
            claude.agent = Some("review-agent".to_string());
            assert!(
                validate_bot_channel_routing_with_thread_parent(
                    &claude,
                    &ProviderKind::Claude,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    Some("review-thread"),
                    parent,
                    false,
                )
                .is_ok(),
                "the directly bound child bot must retain authority"
            );
        });
    }

    #[test]
    fn live_thread_routing_uses_parent_only_when_inheritance_is_enabled() {
        let inherited = routing_role_map(true, false);
        with_temp_root_files(None, Some(&inherited), None, || {
            let codex = bot_settings(ProviderKind::Codex, vec![TEXT_CHANNEL_ID]);
            assert!(
                validate_bot_channel_routing_with_thread_parent(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    Some("child-thread"),
                    Some((ChannelId::new(TEXT_CHANNEL_ID), Some("adk-cdx"))),
                    false,
                )
                .is_ok()
            );
            let child_only = bot_settings(ProviderKind::Codex, vec![THREAD_CHANNEL_ID]);
            assert_eq!(
                validate_bot_channel_routing_with_thread_parent(
                    &child_only,
                    &ProviderKind::Codex,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    Some("child-thread"),
                    Some((ChannelId::new(TEXT_CHANNEL_ID), Some("adk-cdx"))),
                    false,
                ),
                Err(BotChannelRoutingGuardFailure::ChannelNotAllowed),
                "an inherited thread uses parent allowlist authority, not a child-only allowlist"
            );
        });

        let opted_out = routing_role_map(false, false);
        with_temp_root_files(None, Some(&opted_out), None, || {
            let codex = bot_settings(ProviderKind::Codex, vec![TEXT_CHANNEL_ID]);
            assert_eq!(
                validate_bot_channel_routing_with_thread_parent(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    Some("child-thread"),
                    Some((ChannelId::new(TEXT_CHANNEL_ID), Some("adk-cdx"))),
                    false,
                ),
                Err(BotChannelRoutingGuardFailure::ChannelNotAllowed),
                "threadInherit=false must prevent parent allowlist authority"
            );
        });
    }

    #[test]
    fn inherited_parent_still_rejects_wrong_agent_and_wrong_provider() {
        let role_map = routing_role_map(true, false);
        with_temp_root_files(None, Some(&role_map), None, || {
            let parent = Some((ChannelId::new(TEXT_CHANNEL_ID), Some("adk-cdx")));

            let mut wrong_agent = bot_settings(ProviderKind::Codex, vec![TEXT_CHANNEL_ID]);
            wrong_agent.agent = Some("review-agent".to_string());
            assert_eq!(
                validate_bot_channel_routing_with_thread_parent(
                    &wrong_agent,
                    &ProviderKind::Codex,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    Some("child-thread"),
                    parent,
                    false,
                ),
                Err(BotChannelRoutingGuardFailure::AgentMismatch)
            );

            let mut wrong_provider = bot_settings(ProviderKind::Claude, vec![TEXT_CHANNEL_ID]);
            wrong_provider.agent = Some("project-agentdesk".to_string());
            assert_eq!(
                validate_bot_channel_routing_with_thread_parent(
                    &wrong_provider,
                    &ProviderKind::Claude,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    Some("child-thread"),
                    parent,
                    false,
                ),
                Err(BotChannelRoutingGuardFailure::ProviderMismatch)
            );
        });
    }

    #[test]
    fn id_bound_parent_routes_without_name_and_never_borrows_child_name() {
        let role_map = routing_role_map(true, false);
        with_temp_root_files(None, Some(&role_map), None, || {
            let codex = bot_settings(ProviderKind::Codex, vec![TEXT_CHANNEL_ID]);
            assert!(
                validate_bot_channel_routing_with_thread_parent(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    Some("child-name-must-not-be-parent-name"),
                    Some((ChannelId::new(TEXT_CHANNEL_ID), None)),
                    false,
                )
                .is_ok(),
                "an ID-bound parent remains routable when Discord cannot provide its name"
            );

            let mut claude = bot_settings(ProviderKind::Claude, vec![TEXT_CHANNEL_ID]);
            claude.agent = Some("project-agentdesk".to_string());
            assert_eq!(
                validate_bot_channel_routing_with_thread_parent(
                    &claude,
                    &ProviderKind::Claude,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    Some("child-name-must-not-be-parent-name"),
                    Some((ChannelId::new(TEXT_CHANNEL_ID), None)),
                    false,
                ),
                Err(BotChannelRoutingGuardFailure::ProviderMismatch),
                "the child thread name must not contaminate parent provider resolution"
            );
        });
    }

    #[test]
    fn missing_parent_name_cannot_adopt_a_child_named_binding() {
        let role_map = format!(
            r#"{{
  "fallbackByChannelName": {{"enabled": true}},
  "byChannelName": {{
    "child-alias": {{
      "channelId": "{TEXT_CHANNEL_ID}",
      "roleId": "project-agentdesk",
      "promptFile": "/tmp/project-agentdesk.md",
      "provider": "codex"
    }}
  }}
}}"#
        );
        with_temp_root_files(None, Some(&role_map), None, || {
            let mut codex = bot_settings(ProviderKind::Codex, vec![TEXT_CHANNEL_ID]);
            codex.agent = None;
            assert_eq!(
                validate_bot_channel_routing_with_thread_parent(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    Some("child-alias"),
                    Some((ChannelId::new(TEXT_CHANNEL_ID), None)),
                    false,
                ),
                Err(BotChannelRoutingGuardFailure::ProviderMismatch),
                "a child name must not become the missing parent binding/provider identity"
            );
        });
    }

    #[test]
    fn watcher_restore_and_manual_rebind_preserve_child_thread_authority() {
        for source in [
            include_str!("../watchers/lifecycle.rs"),
            include_str!("../recovery_engine/manual_rebind/mod.rs"),
        ] {
            assert_eq!(
                source
                    .matches("settings::validate_bot_channel_routing_with_thread_parent(")
                    .count(),
                1,
                "each recovery intake must use the child-aware thread routing gate exactly once"
            );
            assert!(
                !source.contains("validate_bot_channel_routing_with_provider_channel("),
                "parent-flattening validation must not remain in watcher restore or manual rebind"
            );
            assert!(
                !source.contains("resolve_thread_parent("),
                "child metadata must be fetched once; a second lookup could outrank an unseen direct child binding"
            );
            assert_eq!(
                source
                    .matches("resolve_live_channel_routing_metadata(")
                    .count(),
                1
            );
        }

        let watcher = include_str!("../watchers/lifecycle.rs");
        assert!(!watcher.contains("pname.unwrap_or_else(|| channel_name.clone())"));
        assert_eq!(watcher.matches("live_child_name.as_deref(),").count(), 1);
        assert!(!watcher.contains("Some(&channel_name),\n            thread_parent"));
        let rebind = include_str!("../recovery_engine/manual_rebind/mod.rs");
        assert!(!rebind.contains("pname.or(channel_name.clone())"));
        assert_eq!(rebind.matches("live_child_name.as_deref(),").count(), 1);
        assert!(!rebind.contains("channel_name.as_deref(),\n        thread_parent"));

        let metadata = include_str!("../session_runtime/channel_routing.rs");
        assert!(metadata.contains("return (false, None, None)"));
        assert!(metadata.contains("Some((parent_id, parent_name))"));
    }

    #[test]
    fn live_child_name_binding_wins_but_synthetic_tmux_name_has_no_authority() {
        let role_map = format!(
            r#"{{
  "fallbackByChannelName": {{"enabled": true}},
  "byChannelId": {{
    "{TEXT_CHANNEL_ID}": {{
      "roleId": "project-agentdesk",
      "promptFile": "/tmp/project-agentdesk.md",
      "provider": "codex"
    }}
  }},
  "byChannelName": {{
    "live-child": {{
      "channelId": "{THREAD_CHANNEL_ID}",
      "roleId": "review-agent",
      "promptFile": "/tmp/review-agent.md",
      "provider": "claude"
    }}
  }}
}}"#
        );
        with_temp_root_files(None, Some(&role_map), None, || {
            let mut claude = bot_settings(ProviderKind::Claude, vec![THREAD_CHANNEL_ID]);
            claude.agent = Some("review-agent".to_string());
            let parent = Some((ChannelId::new(TEXT_CHANNEL_ID), Some("adk-cdx")));
            assert!(
                validate_bot_channel_routing_with_thread_parent(
                    &claude,
                    &ProviderKind::Claude,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    Some("live-child"),
                    parent,
                    false,
                )
                .is_ok(),
                "the actual Discord child name may establish direct child authority"
            );
            assert!(
                validate_bot_channel_routing_with_thread_parent(
                    &claude,
                    &ProviderKind::Claude,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    Some("adk-cdx-t1504612455916245999"),
                    parent,
                    false,
                )
                .is_err(),
                "a synthetic tmux/session name must not establish child authority"
            );
            assert!(
                validate_bot_channel_routing_with_thread_parent(
                    &claude,
                    &ProviderKind::Claude,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    None,
                    parent,
                    false,
                )
                .is_err(),
                "failed live child lookup must fail closed for name-only bindings"
            );

            let codex_parent = bot_settings(ProviderKind::Codex, vec![TEXT_CHANNEL_ID]);
            assert_eq!(
                validate_bot_channel_routing_with_thread_parent(
                    &codex_parent,
                    &ProviderKind::Codex,
                    ChannelId::new(THREAD_CHANNEL_ID),
                    None,
                    None,
                    false,
                ),
                Err(BotChannelRoutingGuardFailure::ChannelNotAllowed),
                "unresolved child metadata must not enable a second parent fallback"
            );
        });
    }

    // #3869: the restart recovery engine (`recovery_engine::restore_inflight_turns`)
    // used to `continue` on EVERY routing-validation failure, silently stranding
    // an in-flight row whose channel was re-routed while dcserver was down until
    // the ~1800s placeholder sweeper reaped it. The fix branches on
    // `orphans_inflight_on_restart`: finalize the genuinely-orphaned row, but
    // PRESERVE a row that a same-provider sibling bot can still recover. These
    // tests pin that decision boundary.
    #[test]
    fn restart_orphan_classification_separates_provider_rebind_from_cross_bot_skip() {
        // A ProviderMismatch means the channel was re-bound to a DIFFERENT
        // provider entirely — no same-provider sibling owns the row, so it is
        // genuinely orphaned and must be cleaned up + notified.
        assert!(
            BotChannelRoutingGuardFailure::ProviderMismatch.orphans_inflight_on_restart(),
            "a provider-rebound row is orphaned and must be finalized, not stranded",
        );
        // ChannelNotAllowed / AgentMismatch are `is_expected_cross_bot_skip`
        // outcomes: a sibling bot owns the channel/agent and runs its own
        // recovery pass over the SAME rows, so this bot must PRESERVE the row.
        assert!(
            !BotChannelRoutingGuardFailure::ChannelNotAllowed.orphans_inflight_on_restart(),
            "a not-in-allowlist row is re-routable to a sibling bot and must be preserved",
        );
        assert!(
            !BotChannelRoutingGuardFailure::AgentMismatch.orphans_inflight_on_restart(),
            "an agent-mismatch row is re-routable to a sibling bot and must be preserved",
        );
        // The orphan predicate is exactly the negation of the cross-bot-skip
        // guard — clearing a re-routable row would destroy a sibling's turn.
        for failure in [
            BotChannelRoutingGuardFailure::ChannelNotAllowed,
            BotChannelRoutingGuardFailure::AgentMismatch,
            BotChannelRoutingGuardFailure::ProviderMismatch,
        ] {
            assert_eq!(
                failure.orphans_inflight_on_restart(),
                !failure.is_expected_cross_bot_skip(),
                "orphan-on-restart must be the complement of the cross-bot-skip guard",
            );
        }
    }

    #[test]
    fn restart_routing_change_orphans_provider_rebind_but_preserves_reroutable_and_valid() {
        with_temp_root(|| {
            // (1) STILL-VALID routing: the row's channel is still bound to this
            // bot's provider — `validate` is Ok, so recovery proceeds normally
            // and the row is never handed to the orphan-cleanup path.
            let codex = bot_settings(ProviderKind::Codex, vec![TEXT_CHANNEL_ID]);
            assert!(
                validate_bot_channel_routing(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(TEXT_CHANNEL_ID),
                    Some("adk-cdx"),
                    false,
                )
                .is_ok(),
                "a row whose routing is still valid must recover normally (no cleanup)",
            );

            // (2) PROVIDER RE-BIND (orphaned): a claude bot's row whose channel is
            // now owned by codex. No claude sibling can adopt it → clean up.
            let claude = bot_settings(ProviderKind::Claude, Vec::new());
            let rebind_reason = validate_bot_channel_routing(
                &claude,
                &ProviderKind::Claude,
                ChannelId::new(VOICE_CHANNEL_ID),
                None,
                false,
            )
            .expect_err("claude must be rejected from a codex-owned channel");
            assert_eq!(
                rebind_reason,
                BotChannelRoutingGuardFailure::ProviderMismatch
            );
            assert!(
                rebind_reason.orphans_inflight_on_restart(),
                "#3869: the provider-rebound row must be cleaned up, not stranded for the sweeper",
            );

            // (3) CROSS-BOT SKIP (re-routable): the channel is simply not in this
            // bot's allowlist; the owning sibling bot recovers the row, so it
            // must be PRESERVED (the original bare-`continue` behavior).
            let cross_bot_reason = validate_bot_channel_routing(
                &codex,
                &ProviderKind::Codex,
                ChannelId::new(UNRELATED_CHANNEL_ID),
                None,
                false,
            )
            .expect_err("an unrelated channel must be rejected");
            assert_eq!(
                cross_bot_reason,
                BotChannelRoutingGuardFailure::ChannelNotAllowed
            );
            assert!(
                !cross_bot_reason.orphans_inflight_on_restart(),
                "a re-routable (cross-bot) row must be preserved for the owning sibling bot",
            );
        });
    }
}
