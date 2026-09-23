//! Operator-facing roles, with compatibility at the shared registry boundary.
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum ClusterRole {
    Hub,
    Runner,
    #[default]
    Auto,
}

impl ClusterRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hub => "hub",
            Self::Runner => "runner",
            Self::Auto => "auto",
        }
    }

    /// Keep the v1 shared registry readable by nodes that have not upgraded yet.
    pub(crate) fn registry_value(self) -> &'static str {
        match self {
            Self::Hub => "leader",
            Self::Runner => "worker",
            Self::Auto => "auto",
        }
    }
}

impl FromStr for ClusterRole {
    type Err = &'static str;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "hub" | "leader" => Ok(Self::Hub),
            "runner" | "worker" => Ok(Self::Runner),
            "auto" => Ok(Self::Auto),
            _ => Err("cluster.role must be hub, runner, or auto (legacy aliases: leader, worker)"),
        }
    }
}

impl TryFrom<String> for ClusterRole {
    type Error = &'static str;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl fmt::Display for ClusterRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClusterConfig;

    #[test]
    fn role_aliases_round_trip_to_canonical_configuration() {
        for (input, canonical, registry) in [
            ("hub", "hub", "leader"),
            ("leader", "hub", "leader"),
            ("  LEADER  ", "hub", "leader"),
            ("runner", "runner", "worker"),
            ("worker", "runner", "worker"),
            (" WORKER ", "runner", "worker"),
            ("auto", "auto", "auto"),
        ] {
            let cluster: ClusterConfig = serde_yaml::from_str(&format!("role: '{input}'")).unwrap();
            assert_eq!(cluster.role.as_str(), canonical);
            assert_eq!(cluster.role.registry_value(), registry);
            let json = serde_json::to_value(&cluster).unwrap();
            assert_eq!(json["role"], canonical);
            let reloaded: ClusterConfig = serde_json::from_value(json).unwrap();
            assert_eq!(reloaded.role, cluster.role);
            let yaml = serde_yaml::to_string(&cluster).unwrap();
            assert!(yaml.contains(&format!("role: {canonical}")));
        }
    }

    #[test]
    fn unknown_role_cannot_silently_participate_in_hub_election() {
        for input in ["", "hbu", "runnner", "standby"] {
            assert!(serde_yaml::from_str::<ClusterConfig>(&format!("role: '{input}'")).is_err());
        }
        assert_eq!(ClusterConfig::default().role, ClusterRole::Auto);
    }
}
