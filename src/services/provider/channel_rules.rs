//! Diagnostics for operator-defined channel routing rules.

use super::ProviderKind;
use crate::config::OnboardingConfig;

/// Legacy tmux identity uses the registry default, never live routing policy.
pub(super) fn legacy_default() -> Option<ProviderKind> {
    super::provider_registry()
        .iter()
        .find(|entry| entry.default_channel_provider)
        .and_then(|entry| entry.kind())
}

pub(crate) fn warnings(config: &OnboardingConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut normalized = std::collections::BTreeMap::new();
    for (suffix, provider) in &config.provider_suffix_map {
        let key = suffix.trim().trim_start_matches('-').to_ascii_lowercase();
        if key.is_empty() {
            warnings.push(format!(
                "provider_suffix_map has an empty suffix {suffix:?}; ignored"
            ));
        }
        if let Some(previous) = normalized.insert(key, suffix) {
            warnings.push(format!(
                "provider_suffix_map keys {previous:?} and {suffix:?} normalize to the same suffix"
            ));
        }
        if let Some(provider) = provider {
            if ProviderKind::from_str(provider).is_none() {
                warnings.push(format!(
                    "provider_suffix_map[{suffix:?}] has unknown provider {provider:?}; ignored"
                ));
            }
        }
    }
    if let Some(provider) = &config.default_provider {
        if ProviderKind::from_str(provider).is_none() {
            warnings.push(format!(
                "default_provider has unknown provider {provider:?}; using registry default"
            ));
        }
    }
    let merged = config.merged_provider_suffix_map();
    let suffixes: Vec<_> = merged.keys().collect();
    for (index, left) in suffixes.iter().enumerate() {
        for right in &suffixes[index + 1..] {
            if left.ends_with(right.as_str()) || right.ends_with(left.as_str()) {
                warnings.push(format!("provider_suffix_map suffixes {left:?} and {right:?} overlap; longest exact suffix wins"));
            }
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> OnboardingConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    // Keep nested libtest summaries out of the parent lane's selection accounting.
    fn run_child(name: &str, marker: &str) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(marker, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success()
                && stdout
                    .lines()
                    .filter(|line| line.starts_with("test result:"))
                    .count()
                    == 1
                && stdout.lines().any(|line| line
                    .starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ")),
            "{name}: {}\n{stdout}\n{stderr}",
            output.status
        );
        eprintln!("isolated child verified: {name}; selected=1");
    }

    #[test]
    fn onboarding_routing_legacy_identity_survives_default_changes() {
        if std::env::var_os("ADK_LEGACY_IDENTITY_TEST_CHILD").is_none() {
            run_child(
                "services::provider::channel_rules::tests::onboarding_routing_legacy_identity_survives_default_changes",
                "ADK_LEGACY_IDENTITY_TEST_CHILD",
            );
            return;
        }
        let mut config = crate::config::Config::default();
        for default in ["claude", "qwen", "grok"] {
            config.onboarding.default_provider = Some(default.into());
            crate::config_live_reload::install(config.clone());
            assert_eq!(
                ProviderKind::default_channel_provider(),
                ProviderKind::from_str(default)
            );
            assert_eq!(
                crate::services::provider::parse_provider_and_channel_from_tmux_name(
                    "AgentDesk-legacy-channel"
                ),
                Some((ProviderKind::Claude, "legacy-channel".into()))
            );
        }
    }

    #[test]
    fn onboarding_routing_empty_config_preserves_registry_behavior() {
        let config = parse("{}");
        let historical = [
            ("-cc", "claude"),
            ("-cdx", "codex"),
            ("-gm", "gemini"),
            ("-oc", "opencode"),
            ("-qw", "qwen"),
            ("-gx", "grok"),
        ];
        assert_eq!(config.merged_provider_suffix_map().len(), historical.len());
        for (suffix, provider) in historical {
            assert_eq!(
                config.provider_from_channel_suffix(&format!("dev{suffix}")),
                Some(provider.into())
            );
        }
        for channel in [
            "dev-gem", "dev-cop", "dev-api", "dev-CC", "dev-cc ", "dev", "",
        ] {
            assert_eq!(
                config.provider_from_channel_suffix(channel),
                None,
                "{channel}"
            );
        }
        assert_eq!(
            config.effective_default_provider(),
            Some(ProviderKind::Claude)
        );
        assert!(warnings(&config).is_empty());
    }

    #[test]
    fn onboarding_routing_add_override_delete_and_longest_match() {
        let config = parse(
            "provider_suffix_map: {'-gem': gemini, '-cc': codex, '-gm': null, ' LONG-CC ': qwen}\ndefault_provider: grok",
        );
        assert_eq!(
            config.provider_from_channel_suffix("dev-gem"),
            Some("gemini".into())
        );
        assert_eq!(
            config.provider_from_channel_suffix("dev-cc"),
            Some("codex".into())
        );
        assert_eq!(config.provider_from_channel_suffix("dev-gm"), None);
        assert_eq!(
            config.provider_from_channel_suffix("dev-long-cc"),
            Some("qwen".into())
        );
        assert_eq!(
            config.provider_from_channel_suffix("dev-oc"),
            Some("opencode".into())
        );
        assert_eq!(
            config.effective_default_provider(),
            Some(ProviderKind::Grok)
        );
        let roundtrip: OnboardingConfig =
            serde_yaml::from_str(&serde_yaml::to_string(&config).unwrap()).unwrap();
        assert_eq!(roundtrip, config);
        assert!(
            warnings(&config)
                .iter()
                .any(|line| line.contains("longest exact suffix"))
        );
    }

    #[test]
    fn onboarding_routing_reports_typos_ambiguity_and_normalized_collisions() {
        let config = parse(
            "provider_suffix_map: {'-long-cc': codex, '-cc': claud, 'cc': claude, '': codex}\ndefault_provider: typo",
        );
        let messages = warnings(&config).join("\n");
        assert!(warnings(&parse("provider_suffix_map: {'-m': claude}")).is_empty());
        for expected in [
            "unknown provider",
            "overlap",
            "normalize to the same",
            "empty suffix",
            "default_provider",
        ] {
            assert!(messages.contains(expected), "{messages}");
        }
        let invalid = parse("provider_suffix_map: {'-cc': claud}\ndefault_provider: typo");
        assert_eq!(
            invalid.provider_from_channel_suffix("dev-cc"),
            Some("claude".into())
        );
        assert_eq!(
            invalid.effective_default_provider(),
            Some(ProviderKind::Claude)
        );
        assert!(
            serde_yaml::from_str::<OnboardingConfig>("default_provder: codex")
                .unwrap_err()
                .to_string()
                .contains("unknown field")
        );
    }

    #[test]
    fn onboarding_routing_installed_snapshot_controls_dispatch() {
        const CHILD: &str = "ADK_ONBOARDING_ROUTING_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            run_child(
                "services::provider::channel_rules::tests::onboarding_routing_installed_snapshot_controls_dispatch",
                CHILD,
            );
            return;
        }
        // Load the same validated YAML used at boot; isolate the process-global
        // snapshot so this regression cannot contaminate parallel unit tests.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        std::fs::write(&path, "server: {}\nonboarding:\n  provider_suffix_map: {'-gem': gemini, '-cc': codex, '-gm': null}\n  default_provider: qwen\n").unwrap();
        crate::config_live_reload::install(crate::config::load_from_path(&path).unwrap());
        assert_eq!(
            ProviderKind::from_channel_suffix("dev-gem"),
            Some(ProviderKind::Gemini)
        );
        assert_eq!(
            ProviderKind::from_channel_suffix("dev-cc"),
            Some(ProviderKind::Codex)
        );
        assert_eq!(ProviderKind::from_channel_suffix("dev-gm"), None);
        assert_eq!(
            ProviderKind::default_channel_provider(),
            Some(ProviderKind::Qwen)
        );
        assert_eq!(
            ProviderKind::resolve_channel_provider(Some("dev"), None),
            Some(ProviderKind::Qwen)
        );
        assert!(ProviderKind::Gemini.is_channel_supported(Some("dev-gem"), false, None));
        assert!(!ProviderKind::Claude.is_channel_supported(Some("dev-cc"), false, None));
        assert_eq!(
            ProviderKind::resolve_channel_provider(Some("dev-gem"), Some(&ProviderKind::Claude)),
            Some(ProviderKind::Claude)
        );
    }
}
