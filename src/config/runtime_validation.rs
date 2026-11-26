use super::Config;
use crate::services::provider_hosting::RuntimeMode;

/// Inspect every raw runtime, including shadowed providers and channels without usable IDs.
pub(super) fn validate(config: &Config) -> anyhow::Result<()> {
    let providers = config
        .providers
        .values()
        .filter_map(|provider| provider.runtime.as_deref().and_then(RuntimeMode::parse));
    let channels = config
        .agents
        .iter()
        .flat_map(|agent| agent.channels.iter())
        .filter_map(|(_, channel)| {
            channel
                .runtime_mode_raw()
                .and_then(|raw| RuntimeMode::parse(&raw))
        });
    if providers
        .chain(channels)
        .any(|mode| mode == RuntimeMode::Retired)
    {
        anyhow::bail!("runtime 'claude-e' was removed — delete it from agentdesk.yaml");
    }
    Ok(())
}

/// Fixtures KE·ROOT tests reuse once KRX is its own crate (KRX-TS): the removed-runtime message, the hosting
/// reset guard and the config builders. The unit tests below keep their own cases.
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support {
    use crate::config::{AgentChannel, AgentChannelConfig, AgentDef, Config, ProviderConfig};
    use crate::services::provider_hosting;

    pub(crate) const REMOVED: &str =
        "runtime 'claude-e' was removed — delete it from agentdesk.yaml";

    pub(crate) struct ResetHosting;

    impl Drop for ResetHosting {
        fn drop(&mut self) {
            provider_hosting::install_provider_hosting_config(&Config::default());
        }
    }

    pub(crate) fn fixture(runtime: Option<&str>, channel: bool, provider: &str) -> Config {
        let mut config = Config::default();
        if channel {
            let mut agent: AgentDef = serde_yaml::from_str("id: fixture\nname: Fixture\n").unwrap();
            agent.channels.insert(
                provider,
                AgentChannel::Detailed(AgentChannelConfig {
                    runtime: runtime.map(str::to_owned),
                    ..Default::default()
                }),
            );
            config.agents.push(agent);
        } else {
            config.providers.insert(
                provider.to_owned(),
                ProviderConfig {
                    runtime: runtime.map(str::to_owned),
                    ..Default::default()
                },
            );
        }
        config
    }

    pub(crate) fn write_config(path: &std::path::Path, config: &Config) {
        let mut yaml = serde_yaml::to_value(config).unwrap();
        // The config serializer omits targetless channel maps; validate their raw input too.
        for (index, agent) in config.agents.iter().enumerate() {
            let channels: std::collections::BTreeMap<_, _> = agent.channels.iter().collect();
            yaml["agents"][index]["channels"] = serde_yaml::to_value(channels).unwrap();
        }
        std::fs::write(path, serde_yaml::to_string(&yaml).unwrap()).unwrap();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::test_support::{REMOVED, ResetHosting, fixture, write_config};
    use crate::config::{AgentChannel, AgentChannelConfig, Config};
    use crate::services::provider::ProviderKind;
    use crate::services::provider_hosting::{self, ProviderSessionDriver};

    const ALIASES: &[&str] = &["claude-e", "claude_e", " ClAuDe-E ", "\tCLAUDE_E\t"];

    // Observe the actual supported driver if strict loading silently accepts a removed alias.
    fn strict_rejection(config: &Config) {
        let _lock = provider_hosting::TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _reset = ResetHosting;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate.yaml");
        write_config(&path, config);
        match crate::config::load_from_path(&path) {
            Err(error) => assert!(format!("{error:#}").contains(REMOVED), "{error:#}"),
            Ok(loaded) => {
                provider_hosting::install_provider_hosting_config(&loaded);
                let selection = provider_hosting::resolve_provider_session_selection_with_channel(
                    &ProviderKind::Claude,
                    true,
                    Some(42),
                );
                panic!("removed runtime silently selected a driver: {selection:?}");
            }
        }
    }

    #[test]
    fn strict_load_rejects_retired_provider_runtime_even_when_overridden() {
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        for alias in ALIASES {
            for provider in ["claude", "codex", " Unknown-Provider "] {
                let mut config = fixture(Some(alias), false, provider);
                let mut agent = fixture(Some("pipe"), true, provider).agents.remove(0);
                let channel = agent.channels.get(provider).unwrap().clone();
                if let AgentChannel::Detailed(mut channel) = channel {
                    channel.id = Some("42".to_owned());
                    agent
                        .channels
                        .insert(provider, AgentChannel::Detailed(channel));
                }
                config.agents.push(agent);
                strict_rejection(&config);
            }
        }
    }

    #[test]
    fn strict_load_rejects_retired_channel_runtime_without_valid_target() {
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        for alias in ALIASES {
            for provider in ["claude", "codex", "future-provider"] {
                for id in [None, Some("not-numeric"), Some("42")] {
                    let mut config = fixture(Some(alias), true, provider);
                    config.agents[0].channels.insert(
                        provider,
                        AgentChannel::Detailed(AgentChannelConfig {
                            id: id.map(str::to_owned),
                            provider: Some("unknown-override".to_owned()),
                            runtime: Some((*alias).to_owned()),
                            ..Default::default()
                        }),
                    );
                    strict_rejection(&config);
                }
            }
        }
    }

    #[test]
    fn default_strict_loader_rejects_retired_provider_and_channel_runtime() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate.yaml");
        let _path = crate::config::test_env::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &path,
        );
        for channel in [false, true] {
            for alias in ALIASES {
                write_config(&path, &fixture(Some(alias), channel, "claude"));
                let error = crate::config::load().unwrap_err();
                assert!(format!("{error:#}").contains(REMOVED), "{error:#}");
            }
        }
    }

    #[test]
    fn strict_load_preserves_supported_unknown_and_legacy_selection() {
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _lock = provider_hosting::TEST_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _reset = ResetHosting;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate.yaml");
        for channel in [false, true] {
            for legacy in [false, true] {
                for runtime in [None, Some("pipe"), Some(" TuI "), Some("unknown-runtime")] {
                    let mut config = fixture(runtime, channel, "claude");
                    config
                        .providers
                        .entry("claude".to_owned())
                        .or_default()
                        .tui_hosting = Some(legacy);
                    if channel {
                        config.agents[0].channels.insert(
                            "claude",
                            AgentChannel::Detailed(AgentChannelConfig {
                                id: Some("42".to_owned()),
                                runtime: runtime.map(str::to_owned),
                                ..Default::default()
                            }),
                        );
                    }
                    write_config(&path, &config);
                    let loaded = crate::config::load_from_path(&path).unwrap();
                    provider_hosting::install_provider_hosting_config(&loaded);
                    let selection =
                        provider_hosting::resolve_provider_session_selection_with_channel(
                            &ProviderKind::Claude,
                            true,
                            Some(42),
                        );
                    let tui = match runtime {
                        Some("pipe") => false,
                        Some(" TuI ") => true,
                        _ => legacy,
                    };
                    assert_eq!(selection.requested_tui_hosting, tui, "{runtime:?}");
                    assert_eq!(
                        selection.driver,
                        if tui {
                            ProviderSessionDriver::TuiHosting
                        } else {
                            ProviderSessionDriver::LegacyPrompt
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn strict_load_reports_removed_before_boot_channel_error() {
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let mut config = fixture(Some("claude-e"), false, "claude");
        config.tui_o = Some(serde_yaml::from_str("writer:\n  channels: [999]\n").unwrap());
        strict_rejection(&config);
    }

    #[test]
    fn live_reload_rejects_retired_and_keeps_the_same_snapshot() {
        let _guard = crate::config_live_reload::test_live_guard(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        );
        crate::config_live_reload::install(Config::default());
        let before = crate::config_live_reload::current().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate.yaml");
        for channel in [false, true] {
            for alias in ALIASES {
                write_config(&path, &fixture(Some(alias), channel, "claude"));
                match crate::config_live_reload::reload_from_path(&path) {
                    crate::config_live_reload::ReloadOutcome::Rejected { error } => {
                        assert!(error.contains(REMOVED), "{error}")
                    }
                    outcome => panic!("removed runtime reload: {outcome:?}"),
                }
                assert!(std::sync::Arc::ptr_eq(
                    &before,
                    &crate::config_live_reload::current().unwrap()
                ));
            }
        }
    }
}
