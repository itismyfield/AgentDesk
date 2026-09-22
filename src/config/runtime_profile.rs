//! Boot-time module selection, independent from cluster lease ownership.
use serde::{Deserialize, Serialize};

use super::{ClusterConfig, ClusterIntakeRoutingMode};

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
