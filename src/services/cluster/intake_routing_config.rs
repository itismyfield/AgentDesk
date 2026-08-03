use crate::config::{ClusterIntakeRoutingConfig, ClusterIntakeRoutingMode};

/// How aggressively to apply the Phase-2 routing decision in front of
/// the existing leader intake path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntakeRoutingMode {
    /// Hook is a no-op; the leader runs every intake locally as today.
    /// Default until Phase 5 flips the global flag — keeps prod
    /// behaviour byte-identical while phases 1-4 land.
    Disabled,
    /// The hook does the full decision (label match, worker eligibility,
    /// 23505 classification) but never INSERTs. Logs the decision so
    /// operators can verify routing behaviour against real traffic
    /// before promoting to `Enforce`.
    Observe,
    /// The hook actually INSERTs into `intake_outbox` and tells the
    /// caller to skip local execution.
    Enforce,
}

impl IntakeRoutingMode {
    fn from_config(mode: ClusterIntakeRoutingMode) -> Self {
        match mode {
            ClusterIntakeRoutingMode::Disabled => Self::Disabled,
            ClusterIntakeRoutingMode::Observe => Self::Observe,
            ClusterIntakeRoutingMode::Enforce => Self::Enforce,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Observe => "observe",
            Self::Enforce => "enforce",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntakeRoutingModeSource {
    ConfigYaml,
    EnvOverride,
}

impl IntakeRoutingModeSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ConfigYaml => "yaml",
            Self::EnvOverride => "env_override",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntakeRoutingEnvOverride {
    Disabled,
    Observe,
    Enforce,
    Invalid,
}

impl IntakeRoutingEnvOverride {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Observe => "observe",
            Self::Enforce => "enforce",
            Self::Invalid => "invalid",
        }
    }

    fn mode(self) -> IntakeRoutingMode {
        match self {
            Self::Disabled | Self::Invalid => IntakeRoutingMode::Disabled,
            Self::Observe => IntakeRoutingMode::Observe,
            Self::Enforce => IntakeRoutingMode::Enforce,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnerAuthorityChannelOptIn {
    OptedIn,
    NotOptedIn,
    Unknown,
}

impl OwnerAuthorityChannelOptIn {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::OptedIn => "opted_in",
            Self::NotOptedIn => "not_opted_in",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EffectiveIntakeRoutingConfig {
    pub(crate) mode: IntakeRoutingMode,
    pub(crate) source: IntakeRoutingModeSource,
    pub(crate) yaml_enabled: bool,
    pub(crate) yaml_mode: ClusterIntakeRoutingMode,
    pub(crate) env_override: Option<&'static str>,
    pub(crate) warnings: Vec<&'static str>,
    pub(crate) owner_authority_channel_ids: Vec<String>,
    pub(crate) owner_authority_config_known: bool,
    pub(crate) forward_pre_claim_timeout_secs: u64,
    pub(crate) stale_claim_recovery_secs: u64,
    pub(crate) max_attempts_per_message: u32,
    pub(crate) retry_authorization_secs: u64,
}

impl EffectiveIntakeRoutingConfig {
    pub(crate) fn mode_is_enforce(&self) -> bool {
        matches!(self.mode, IntakeRoutingMode::Enforce)
    }

    pub(crate) fn worker_consumer_should_spawn(&self) -> bool {
        !matches!(self.mode, IntakeRoutingMode::Disabled)
    }

    pub(crate) fn owner_authority_channel_opt_in(
        &self,
        channel_id: &str,
    ) -> OwnerAuthorityChannelOptIn {
        if !self.owner_authority_config_known {
            return OwnerAuthorityChannelOptIn::Unknown;
        }
        if self
            .owner_authority_channel_ids
            .iter()
            .any(|configured| configured.trim() == channel_id)
        {
            OwnerAuthorityChannelOptIn::OptedIn
        } else {
            OwnerAuthorityChannelOptIn::NotOptedIn
        }
    }

    pub(crate) fn status_json(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": !matches!(self.mode, IntakeRoutingMode::Disabled),
            "mode": self.mode.as_str(),
            "source": self.source.as_str(),
            "owner_authority_allowlist_size": self.owner_authority_channel_ids.len(),
            "owner_authority_config_state": if self.owner_authority_config_known { "known" } else { "unknown" },
            "recent_decision_count": super::intake_routing_telemetry::recent_decision_count(),
            "yaml": {
                "enabled": self.yaml_enabled,
                "mode": self.yaml_mode.as_str(),
                "owner_authority_allowlist_size": self.owner_authority_channel_ids.len(),
                "forward_pre_claim_timeout_secs": self.forward_pre_claim_timeout_secs,
                "stale_claim_recovery_secs": self.stale_claim_recovery_secs,
                "retry_authorization_secs": self.retry_authorization_secs,
            },
            "env_override": self.env_override,
            "warning_count": self.warnings.len(),
            "configuration_warnings": self.warnings,
        })
    }
}

fn parse_intake_routing_env_override(value: &str) -> IntakeRoutingEnvOverride {
    match value.trim().to_ascii_lowercase().as_str() {
        "disabled" | "disable" | "off" | "false" | "0" => IntakeRoutingEnvOverride::Disabled,
        "observe" => IntakeRoutingEnvOverride::Observe,
        "enforce" => IntakeRoutingEnvOverride::Enforce,
        _ => IntakeRoutingEnvOverride::Invalid,
    }
}

fn effective_intake_routing_config_for_with_known(
    config: &ClusterIntakeRoutingConfig,
    env_override: Option<&str>,
    owner_authority_config_known: bool,
) -> EffectiveIntakeRoutingConfig {
    let yaml_mode = if config.enabled {
        IntakeRoutingMode::from_config(config.mode)
    } else {
        IntakeRoutingMode::Disabled
    };
    let parsed_env = env_override.map(parse_intake_routing_env_override);
    let (mode, source) = match parsed_env {
        Some(value) => (value.mode(), IntakeRoutingModeSource::EnvOverride),
        None => (yaml_mode, IntakeRoutingModeSource::ConfigYaml),
    };
    let mut warnings = Vec::new();
    if parsed_env == Some(IntakeRoutingEnvOverride::Invalid) {
        warnings.push("invalid_ADK_INTAKE_ROUTING_MODE_fail_closed");
    }
    if !owner_authority_config_known {
        warnings.push("owner_authority_config_unknown_fail_closed");
    }
    EffectiveIntakeRoutingConfig {
        mode,
        source,
        yaml_enabled: config.enabled,
        yaml_mode: config.mode,
        env_override: parsed_env.map(IntakeRoutingEnvOverride::as_str),
        warnings,
        owner_authority_config_known,
        owner_authority_channel_ids: config.owner_authority_channel_ids.clone(),
        forward_pre_claim_timeout_secs: config.forward_pre_claim_timeout_secs,
        stale_claim_recovery_secs: config.stale_claim_recovery_secs,
        max_attempts_per_message: config.max_attempts_per_message,
        retry_authorization_secs: config.retry_authorization_secs,
    }
}

fn effective_intake_routing_config_for(
    config: &ClusterIntakeRoutingConfig,
    env_override: Option<&str>,
) -> EffectiveIntakeRoutingConfig {
    effective_intake_routing_config_for_with_known(config, env_override, true)
}

pub(crate) fn effective_intake_routing_config() -> EffectiveIntakeRoutingConfig {
    let (config, owner_authority_config_known) = match crate::config_live_reload::current() {
        Some(config) => (config.cluster.intake_routing.clone(), true),
        None => {
            let path = crate::config::resolved_config_path();
            match crate::config::load_from_path(&path) {
                Ok(config) => (config.cluster.intake_routing, true),
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "[intake_router] routing config unavailable; keeping owner-authority fence"
                    );
                    (ClusterIntakeRoutingConfig::default(), false)
                }
            }
        }
    };
    let env_override = std::env::var("ADK_INTAKE_ROUTING_MODE").ok();
    effective_intake_routing_config_for_with_known(
        &config,
        env_override.as_deref(),
        owner_authority_config_known,
    )
}

pub(crate) fn intake_routing_status_json() -> serde_json::Value {
    effective_intake_routing_config().status_json()
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn effective_intake_routing_config_resolves_yaml_and_env_override() {
        let mut yaml = ClusterIntakeRoutingConfig::default();
        assert_eq!(
            effective_intake_routing_config_for(&yaml, None).mode,
            IntakeRoutingMode::Disabled
        );

        yaml.enabled = true;
        assert_eq!(
            effective_intake_routing_config_for(&yaml, None).mode,
            IntakeRoutingMode::Observe
        );

        yaml.mode = ClusterIntakeRoutingMode::Enforce;
        yaml.owner_authority_channel_ids = vec!["123".into()];
        let from_yaml = effective_intake_routing_config_for(&yaml, None);
        assert_eq!(from_yaml.mode, IntakeRoutingMode::Enforce);
        assert_eq!(from_yaml.source, IntakeRoutingModeSource::ConfigYaml);
        assert_eq!(
            from_yaml.owner_authority_channel_opt_in("123"),
            OwnerAuthorityChannelOptIn::OptedIn
        );
        assert_eq!(
            from_yaml.owner_authority_channel_opt_in("456"),
            OwnerAuthorityChannelOptIn::NotOptedIn
        );

        let env_observe = effective_intake_routing_config_for(&yaml, Some("observe"));
        assert_eq!(env_observe.mode, IntakeRoutingMode::Observe);
        assert_eq!(env_observe.source, IntakeRoutingModeSource::EnvOverride);
        assert_eq!(env_observe.env_override, Some("observe"));
        assert_eq!(
            env_observe.owner_authority_channel_opt_in("123"),
            OwnerAuthorityChannelOptIn::OptedIn
        );
        assert_eq!(
            env_observe.owner_authority_channel_opt_in("456"),
            OwnerAuthorityChannelOptIn::NotOptedIn
        );

        let env_disabled = effective_intake_routing_config_for(&yaml, Some("OFF"));
        assert_eq!(env_disabled.mode, IntakeRoutingMode::Disabled);
        assert_eq!(env_disabled.env_override, Some("disabled"));

        let invalid = effective_intake_routing_config_for(&yaml, Some("garbage"));
        assert_eq!(invalid.mode, IntakeRoutingMode::Disabled);
        assert_eq!(invalid.source, IntakeRoutingModeSource::EnvOverride);
        assert_eq!(invalid.env_override, Some("invalid"));
        assert_eq!(
            invalid.warnings,
            vec!["invalid_ADK_INTAKE_ROUTING_MODE_fail_closed"]
        );

        let unknown = effective_intake_routing_config_for_with_known(&yaml, Some("enforce"), false);
        assert_eq!(
            unknown.owner_authority_channel_opt_in("123"),
            OwnerAuthorityChannelOptIn::Unknown
        );
        assert!(
            unknown
                .warnings
                .contains(&"owner_authority_config_unknown_fail_closed")
        );
    }
}
