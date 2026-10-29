use serde::{Deserialize, Serialize};

use super::profile::{Observation, ResourceIdentity, ResourceKind};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceFact {
    pub identity: String,
    pub path: Option<String>,
    pub version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceObservation {
    pub resource: ResourceIdentity,
    pub observation: Observation<ResourceFact>,
}

impl ResourceObservation {
    pub fn is_available(&self) -> bool {
        self.observation.observed_evidence().is_some()
    }
}

// build_token.py exit codes: a token wait is a resource outcome, never a quality failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildTokenOutcome {
    Held,
    Timeout,
    Unusable,
    Nested,
    Usage,
}

pub fn classify_build_token_exit(exit_code: Option<i32>) -> Option<BuildTokenOutcome> {
    match exit_code {
        Some(75) => Some(BuildTokenOutcome::Timeout),
        Some(69) => Some(BuildTokenOutcome::Unusable),
        Some(73) => Some(BuildTokenOutcome::Nested),
        Some(64) => Some(BuildTokenOutcome::Usage),
        Some(_) => Some(BuildTokenOutcome::Held),
        None => None,
    }
}

pub fn resource(kind: ResourceKind, host: &str, identity: &str) -> ResourceIdentity {
    ResourceIdentity {
        kind,
        host: host.into(),
        identity: identity.into(),
    }
}

#[cfg(test)]
#[path = "adk_resources_tests.rs"]
mod tests;
