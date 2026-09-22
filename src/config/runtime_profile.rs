//! Boot-time module selection, independent from cluster lease ownership.
use serde::{Deserialize, Serialize};

use super::*;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProfile {
    #[default]
    Full,
    Worker,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct RuntimeModulePlan {
    pub gateway: bool,
    pub voice: bool,
    pub dashboard: bool,
    pub admin_api: bool,
    pub leader_services: bool,
}

impl RuntimeProfile {
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    pub fn modules(self) -> RuntimeModulePlan {
        let full = self.is_full();
        RuntimeModulePlan {
            gateway: full,
            voice: full,
            dashboard: full,
            admin_api: full,
            leader_services: full,
        }
    }

    pub(super) fn validate(self, cluster: &ClusterConfig) -> anyhow::Result<()> {
        if self == Self::Worker {
            anyhow::ensure!(
                cluster.enabled && cluster.role.trim().eq_ignore_ascii_case("worker"),
                "cluster.runtime_profile=worker requires cluster.enabled=true and role=worker"
            );
            anyhow::ensure!(
                cluster.intake_routing.enabled
                    && cluster.intake_routing.mode != ClusterIntakeRoutingMode::Disabled,
                "cluster.runtime_profile=worker requires enabled intake routing in observe or enforce mode"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_profile_is_explicit_validated_and_does_not_change_legacy_roles() {
        for role in ["leader", "auto", "worker"] {
            let legacy: ClusterConfig = serde_yaml::from_str(&format!("role: {role}")).unwrap();
            assert_eq!(legacy.runtime_profile, RuntimeProfile::Full);
            assert!(legacy.runtime_profile.modules().gateway);
            assert!(legacy.runtime_profile.validate(&legacy).is_ok());
        }
        assert!(serde_yaml::from_str::<ClusterConfig>("runtime_profile: typo").is_err());
        let mut cluster = ClusterConfig {
            runtime_profile: RuntimeProfile::Worker,
            enabled: true,
            role: "worker".into(),
            ..Default::default()
        };
        assert!(cluster.runtime_profile.validate(&cluster).is_err());
        cluster.intake_routing.enabled = true;
        assert!(cluster.runtime_profile.validate(&cluster).is_ok());
        assert_eq!(
            serde_json::to_value(cluster.runtime_profile.modules()).unwrap(),
            serde_json::json!({"gateway":false,"voice":false,"dashboard":false,"admin_api":false,"leader_services":false})
        );
        cluster.role = "auto".into();
        assert!(cluster.runtime_profile.validate(&cluster).is_err());
        cluster.role = "worker".into();
        cluster.enabled = false;
        assert!(cluster.runtime_profile.validate(&cluster).is_err());
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClusterConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(default = "default_cluster_role")]
    pub role: String,
    #[serde(default, skip_serializing_if = "RuntimeProfile::is_full")]
    pub runtime_profile: RuntimeProfile,
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
            role: default_cluster_role(),
            runtime_profile: RuntimeProfile::default(),
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
