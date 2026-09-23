//! Boot-time module selection, independent from cluster lease ownership.
use serde::{Deserialize, Serialize};

use super::*;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProfile {
    #[default]
    Full,
    #[serde(alias = "worker")]
    Runner,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct RuntimeModulePlan {
    pub gateway: bool,
    pub voice: bool,
    pub dashboard: bool,
    pub admin_api: bool,
    #[serde(rename = "leader_services")]
    pub hub_services: bool,
}

impl RuntimeProfile {
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    pub(crate) fn standby_notice(self) -> String {
        format!("  ▸ Cluster runtime {self:?}: keeping HTTP and runner heartbeat online")
    }

    pub fn modules(self) -> RuntimeModulePlan {
        let full = self.is_full();
        RuntimeModulePlan {
            gateway: full,
            voice: full,
            dashboard: full,
            admin_api: full,
            hub_services: full,
        }
    }

    pub(super) fn validate(self, cluster: &ClusterConfig) -> anyhow::Result<()> {
        if let Some(slots) = cluster.execution_slots {
            anyhow::ensure!(
                cluster.enabled && (1..=1024).contains(&slots),
                "execution_slots requires enabled cluster and a value from 1 to 1024"
            );
        }
        if cluster.intake_routing.capacity_aware {
            anyhow::ensure!(
                cluster.enabled && cluster.intake_routing.enabled,
                "capacity_aware requires cluster and intake routing"
            );
        }
        if self == Self::Runner {
            anyhow::ensure!(
                cluster.enabled && cluster.role == ClusterRole::Runner,
                "cluster.runtime_profile=runner requires cluster.enabled=true and role=runner"
            );
            anyhow::ensure!(
                cluster.intake_routing.enabled
                    && cluster.intake_routing.mode != ClusterIntakeRoutingMode::Disabled,
                "cluster.runtime_profile=runner requires enabled intake routing in observe or enforce mode"
            );
        }
        Ok(())
    }

    /// Schema-1 probes are also consumed by nodes that have not upgraded yet.
    pub(crate) fn serialize_registry<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            Self::Full => "full",
            Self::Runner => "worker",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_cluster_roles_keep_single_machine_full_runtime() {
        let cluster: ClusterConfig = serde_yaml::from_str("{}").unwrap();
        assert!(!cluster.enabled);
        assert_eq!(cluster.runtime_profile, RuntimeProfile::Full);
        assert!(cluster.runtime_profile.modules().gateway);
        assert!(cluster.runtime_profile.modules().admin_api);
        assert!(cluster.runtime_profile.modules().dashboard);
        assert!(!cluster.intake_routing.capacity_aware);
        assert_eq!(cluster.execution_slots, None);
        assert!(cluster.runtime_profile.validate(&cluster).is_ok());
    }

    #[test]
    fn runner_profile_is_explicit_validated_and_does_not_change_legacy_roles() {
        for role in ["hub", "runner", "leader", "auto", "worker"] {
            let legacy: ClusterConfig = serde_yaml::from_str(&format!("role: {role}")).unwrap();
            assert_eq!(legacy.runtime_profile, RuntimeProfile::Full);
            assert!(legacy.runtime_profile.modules().gateway);
            assert!(legacy.runtime_profile.validate(&legacy).is_ok());
        }
        assert!(serde_yaml::from_str::<ClusterConfig>("runtime_profile: typo").is_err());
        let mut cluster = ClusterConfig {
            runtime_profile: RuntimeProfile::Runner,
            enabled: true,
            role: ClusterRole::Runner,
            ..Default::default()
        };
        assert!(cluster.runtime_profile.validate(&cluster).is_err());
        cluster.intake_routing.enabled = true;
        assert!(cluster.runtime_profile.validate(&cluster).is_ok());
        assert_eq!(
            serde_json::to_value(cluster.runtime_profile.modules()).unwrap(),
            serde_json::json!({"gateway":false,"voice":false,"dashboard":false,"admin_api":false,"leader_services":false})
        );
        cluster.role = ClusterRole::Auto;
        assert!(cluster.runtime_profile.validate(&cluster).is_err());
        cluster.role = ClusterRole::Runner;
        cluster.enabled = false;
        assert!(cluster.runtime_profile.validate(&cluster).is_err());
    }

    #[test]
    fn runner_and_legacy_profiles_have_identical_modules_and_canonical_output() {
        for role in ["runner", "worker"] {
            for profile in ["runner", "worker"] {
                let yaml = format!(
                    "enabled: true\nrole: {role}\nruntime_profile: {profile}\nintake_routing:\n  enabled: true\n  mode: enforce\n"
                );
                let cluster: ClusterConfig = serde_yaml::from_str(&yaml).unwrap();
                assert!(cluster.runtime_profile.validate(&cluster).is_ok());
                assert_eq!(cluster.runtime_profile, RuntimeProfile::Runner);
                assert!(!cluster.runtime_profile.modules().gateway);
                assert!(!cluster.runtime_profile.modules().hub_services);
                let value = serde_json::to_value(&cluster).unwrap();
                assert_eq!(value["role"], "runner");
                assert_eq!(value["runtime_profile"], "runner");
                let reloaded: ClusterConfig = serde_json::from_value(value).unwrap();
                assert_eq!(reloaded, cluster);
            }
        }
        for role in [ClusterRole::Hub, ClusterRole::Auto] {
            let mut cluster = ClusterConfig {
                enabled: true,
                role,
                runtime_profile: RuntimeProfile::Runner,
                ..Default::default()
            };
            cluster.intake_routing.enabled = true;
            assert!(cluster.runtime_profile.validate(&cluster).is_err());
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClusterConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub role: ClusterRole,
    #[serde(default, skip_serializing_if = "RuntimeProfile::is_full")]
    pub runtime_profile: RuntimeProfile,
    /// Maximum simultaneous provider turns on this node; restart to change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_slots: Option<u32>,
    #[serde(default = "default_cluster_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,
    #[serde(default = "default_cluster_lease_ttl_secs")]
    pub lease_ttl_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    /// #4351: instance that should own the Discord gateway singleton lease — in
    /// practice, the node every conversational tmux session runs on. `None` keeps
    /// the pre-#4351 first-come behavior. Yield protocol and failover semantics:
    /// `discord::runtime_bootstrap::gateway_lease`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_preferred_instance_id: Option<String>,
    /// #4351: how long a non-preferred node stands by for the preferred node
    /// before taking the lease itself. Only consulted while the preferred node is
    /// online and advertising gateway intent.
    #[serde(default = "default_gateway_yield_grace_secs")]
    pub gateway_yield_grace_secs: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub capabilities: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub nodes: BTreeMap<String, ClusterNodeConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub blackout_windows: BTreeMap<String, Vec<ClusterBlackoutWindowConfig>>,
    #[serde(
        default,
        skip_serializing_if = "ClusterDispatchRoutingConfig::is_default"
    )]
    pub dispatch_routing: ClusterDispatchRoutingConfig,
    #[serde(
        default,
        skip_serializing_if = "ClusterIntakeRoutingConfig::is_default"
    )]
    pub intake_routing: ClusterIntakeRoutingConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub semaphores: BTreeMap<String, ClusterSemaphoreConfig>,
    /// Epic #2285 / E3 + E4 + E5 gate. When `true` (default since E5 / #2412),
    /// the session-bound `WatcherSupervisor` + `StreamRelay` infrastructure runs
    /// in production with a Discord `RelaySink`, and the production tmux frame
    /// producer (`services::discord::tmux::tmux_watcher`) pushes every chunk it reads
    /// into the supervisor-owned relay via `RelayProducerRegistry`. The
    /// session-bound sink owns Discord terminal delivery for eligible inflight
    /// shapes (rebind-origin/adopted sessions and watcher-owned relays); the
    /// legacy watcher remains a fallback for bridge-owned/no-inflight envelopes
    /// and runtimes that have no Discord health registry. Setting the flag to
    /// `false` skips the supervisor entirely and the producer-side lookups
    /// become silent no-ops (the registry stays empty), restoring pre-E5
    /// behavior.
    #[serde(default = "default_session_bound_relay_enabled")]
    pub session_bound_relay_enabled: bool,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            instance_id: None,
            role: ClusterRole::default(),
            runtime_profile: RuntimeProfile::default(),
            execution_slots: None,
            heartbeat_interval_secs: default_cluster_heartbeat_interval_secs(),
            lease_ttl_secs: default_cluster_lease_ttl_secs(),
            api_base_url: None,
            gateway_preferred_instance_id: None,
            gateway_yield_grace_secs: default_gateway_yield_grace_secs(),
            labels: Vec::new(),
            capabilities: serde_json::Map::new(),
            nodes: BTreeMap::new(),
            blackout_windows: BTreeMap::new(),
            dispatch_routing: ClusterDispatchRoutingConfig::default(),
            intake_routing: ClusterIntakeRoutingConfig::default(),
            semaphores: BTreeMap::new(),
            session_bound_relay_enabled: default_session_bound_relay_enabled(),
        }
    }
}

impl ClusterConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ClusterIntakeRoutingConfig {
    #[serde(default)]
    pub capacity_aware: bool,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "ClusterIntakeRoutingMode::is_default")]
    pub mode: ClusterIntakeRoutingMode,
    /// Raw top-level Discord channel IDs opted into owner-authority planning.
    /// A valid loaded config with an empty list is an explicit known-empty
    /// opt-out scope; a config that failed to load is represented as unknown by
    /// the effective routing snapshot instead of by this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_authority_channel_ids: Vec<String>,
    #[serde(default = "default_intake_forward_pre_claim_timeout_secs")]
    pub forward_pre_claim_timeout_secs: u64,
    #[serde(default = "default_intake_stale_claim_recovery_secs")]
    pub stale_claim_recovery_secs: u64,
    #[serde(default = "default_intake_max_attempts_per_message")]
    pub max_attempts_per_message: u32,
    #[serde(default = "default_intake_retry_authorization_secs")]
    pub retry_authorization_secs: u64,
}

impl Default for ClusterIntakeRoutingConfig {
    fn default() -> Self {
        Self {
            capacity_aware: false,
            enabled: false,
            mode: ClusterIntakeRoutingMode::default(),
            owner_authority_channel_ids: Vec::new(),
            forward_pre_claim_timeout_secs: default_intake_forward_pre_claim_timeout_secs(),
            stale_claim_recovery_secs: default_intake_stale_claim_recovery_secs(),
            max_attempts_per_message: default_intake_max_attempts_per_message(),
            retry_authorization_secs: default_intake_retry_authorization_secs(),
        }
    }
}
