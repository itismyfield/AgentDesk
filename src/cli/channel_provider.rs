//! Config-aware provider inference for the offline session-name command.

use crate::{config, services::provider::ProviderKind};

pub(super) fn from_config(channel: &str) -> Result<Option<ProviderKind>, String> {
    let path = config::resolved_config_path();
    from_path(channel, &path)
}

fn from_path(channel: &str, path: &std::path::Path) -> Result<Option<ProviderKind>, String> {
    let onboarding = if path.try_exists().map_err(|error| error.to_string())? {
        config::load_from_path(&path)
            .map_err(|error| format!("{error:#}"))?
            .onboarding
    } else {
        config::OnboardingConfig::default()
    };
    Ok(onboarding
        .provider_from_channel_suffix(channel)
        .as_deref()
        .and_then(ProviderKind::from_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onboarding_routing_cli_reads_config_and_rejects_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentdesk.yaml");
        assert_eq!(
            from_path("dev-cc", &path).unwrap(),
            Some(ProviderKind::Claude)
        );
        std::fs::write(
            &path,
            "server: {}\nonboarding:\n  provider_suffix_map: {'-new': codex, '-cc': null}\n",
        )
        .unwrap();
        assert_eq!(
            from_path("dev-new", &path).unwrap(),
            Some(ProviderKind::Codex)
        );
        assert_eq!(from_path("dev-cc", &path).unwrap(), None);
        std::fs::write(&path, "server: {}\nonboarding:\n  provider_sufix_map: {}\n").unwrap();
        assert!(
            from_path("dev-cc", &path)
                .unwrap_err()
                .contains("unknown field")
        );
    }
}
