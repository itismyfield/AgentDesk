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
    None
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
    None
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

    // A normal text-channel binding for the owning (codex) agent.
    const TEXT_CHANNEL_ID: u64 = 1479671301387059200;
    // An unrelated channel that is not in the allowlist.
    const UNRELATED_CHANNEL_ID: u64 = 1111111111111111111;

    fn with_temp_root<F>(f: F)
    where
        F: FnOnce(&std::path::Path),
    {
        // Serialize on the process-wide `AGENTDESK_ROOT_DIR` lock so this
        // root-mutating helper cannot race a concurrent test in another module.
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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
    channels:
      codex:
        id: "1479671301387059200"
        name: "adk-cdx"
"#,
        )
        .unwrap();
        let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            &root,
        );
        f(&root);
    }

    use crate::test_env_panic_probe::{assert_root_restored, checkpoint};

    fn exercise_validation_runtime_root() {
        with_temp_root(|root| checkpoint(&[("AGENTDESK_ROOT_DIR", root.as_os_str())]))
    }

    #[test]
    fn validation_runtime_root_restores_env_after_panic_present() {
        assert_root_restored(true, exercise_validation_runtime_root);
    }

    #[test]
    fn validation_runtime_root_restores_env_after_panic_absent() {
        assert_root_restored(false, exercise_validation_runtime_root);
    }

    fn bot_settings(provider: ProviderKind, allowed_channel_ids: Vec<u64>) -> DiscordBotSettings {
        DiscordBotSettings {
            provider,
            allowed_channel_ids,
            agent: Some("project-agentdesk".to_string()),
            ..Default::default()
        }
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
        with_temp_root(|_root| {
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
                ChannelId::new(TEXT_CHANNEL_ID),
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

#[cfg(test)]
mod retirement_tests {
    use super::*;

    #[test]
    fn yaml_and_org_bindings_win_and_legacy_role_map_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _live = crate::config_live_reload::test_live_guard(lock);
        let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        crate::config_live_reload::install(crate::config::Config::default());
        assert!(crate::config::AgentChannelConfig::default().thread_inherit);
        let channel = ChannelId::new(73_490_101);
        let legacy = root.path().join("config/role_map.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let shared = root.path().join("legacy-shared.md");
        std::fs::write(&shared, "legacy guidance").unwrap();
        std::fs::write(
            &legacy,
            serde_json::json!({
                "byChannelId": { channel.to_string(): {
                    "roleId": "legacy-agent", "promptFile": "/legacy.md", "provider": "codex",
                    "workspace": "/legacy-workspace"
                }},
                "sharedPromptFile": shared,
                "meeting": {"channel_name": "legacy-meeting", "summary_agent": "legacy-agent"}
            })
            .to_string(),
        )
        .unwrap();
        assert!(resolve_role_binding(channel, None).is_none());
        assert!(resolve_workspace(channel, None).is_none());
        assert!(list_registered_channel_bindings().is_empty());
        assert!(!super::super::content::is_known_agent("legacy-agent"));
        assert!(super::super::content::load_shared_prompt_for_profile("claude").is_none());
        assert!(crate::services::discord::meeting_orchestrator::load_meeting_config().is_none());

        let org = crate::runtime_layout::org_schema_path(root.path());
        std::fs::write(
            &org,
            serde_yaml::to_string(&serde_json::json!({
                "version": 1,
                "agents": {"org-agent": {"display_name": "Org", "provider": "claude",
                    "prompt_file": "/org.md", "workspace": "/org-workspace"}},
                "channels": {"by_id": {channel.to_string(): {"agent": "org-agent"}}},
                "meeting": {"channel_name": "org-meeting", "summary_agent": "org-agent"}
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            resolve_role_binding(channel, None).unwrap().role_id,
            "org-agent"
        );
        assert_eq!(
            resolve_workspace(channel, None).as_deref(),
            Some("/org-workspace")
        );
        assert_eq!(
            crate::services::discord::meeting_orchestrator::load_meeting_config()
                .unwrap()
                .channel_name,
            "org-meeting"
        );

        let mut config = crate::config::Config::default();
        config.agents = serde_json::from_value(serde_json::json!([{
            "id": "yaml-agent", "name": "YAML", "provider": "codex", "channels": {
                "codex": {"id": channel.to_string(), "workspace": "/yaml-workspace"}
            }
        }]))
        .unwrap();
        config.meeting = Some(
            serde_json::from_value(serde_json::json!({
                "channel_name": "yaml-meeting", "summary_agent": "yaml-agent"
            }))
            .unwrap(),
        );
        crate::config_live_reload::install(config);
        assert_eq!(
            resolve_role_binding(channel, None).unwrap().role_id,
            "yaml-agent"
        );
        assert_eq!(
            resolve_workspace(channel, None).as_deref(),
            Some("/yaml-workspace")
        );
        assert_eq!(list_registered_channel_bindings().len(), 1);
        assert_eq!(
            list_registered_channel_bindings()[0].owner_provider,
            ProviderKind::Codex
        );
        assert_eq!(
            crate::services::discord::meeting_orchestrator::load_meeting_config()
                .unwrap()
                .channel_name,
            "yaml-meeting"
        );
    }
}
