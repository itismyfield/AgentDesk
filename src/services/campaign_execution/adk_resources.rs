use std::path::Path;

use chrono::{DateTime, Utc};
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

pub fn observe_tool(host: &str, tool: &str, observed_at: DateTime<Utc>) -> ResourceObservation {
    let resource = resource(ResourceKind::Host, host, tool);
    let observation = match which::which(tool) {
        Ok(path) => Observation::Observed {
            evidence: ResourceFact {
                identity: tool.into(),
                path: Some(path.to_string_lossy().into_owned()),
                version: None,
            },
            observed_at,
        },
        Err(error) => Observation::NotYet {
            reason: format!("{tool} not on PATH: {error}"),
            observed_at,
        },
    };
    ResourceObservation {
        resource,
        observation,
    }
}

// A target directory is observed only when it exists and accepts a write; absence is NotYet.
pub fn observe_target_dir(
    host: &str,
    target: &Path,
    observed_at: DateTime<Utc>,
) -> ResourceObservation {
    let identity = target.to_string_lossy().into_owned();
    let resource = resource(ResourceKind::Build, host, &identity);
    let observation = match std::fs::create_dir_all(target)
        .and_then(|_| tempfile::tempfile_in(target))
        .and_then(|mut probe| std::io::Write::write_all(&mut probe, b"probe"))
    {
        Ok(()) => Observation::Observed {
            evidence: ResourceFact {
                identity: identity.clone(),
                path: Some(identity),
                version: None,
            },
            observed_at,
        },
        Err(error) => Observation::NotYet {
            reason: format!("target dir not writable: {error}"),
            observed_at,
        },
    };
    ResourceObservation {
        resource,
        observation,
    }
}

// The token file holder cannot be read without taking the token; presence alone is Unknown.
pub fn observe_build_token(
    host: &str,
    token_path: &Path,
    observed_at: DateTime<Utc>,
) -> ResourceObservation {
    let identity = token_path.to_string_lossy().into_owned();
    let observation = if token_path.exists() {
        Observation::Unknown {
            reason: "build token presence does not show who holds it".into(),
            last_observed_at: Some(observed_at),
        }
    } else {
        Observation::NotYet {
            reason: "build token path absent".into(),
            observed_at,
        }
    };
    ResourceObservation {
        resource: resource(ResourceKind::Build, host, &identity),
        observation,
    }
}

pub fn observe_pg(
    host: &str,
    url: Option<&str>,
    observed_at: DateTime<Utc>,
) -> ResourceObservation {
    let resource = resource(ResourceKind::Host, host, "isolated-postgres");
    let observation = match url.map(str::trim).filter(|u| !u.is_empty()) {
        Some(url) if url.starts_with("postgres://") || url.starts_with("postgresql://") => {
            Observation::Unknown {
                reason: "isolated PG URL configured; reachability not probed".into(),
                last_observed_at: Some(observed_at),
            }
        }
        Some(_) => Observation::NotYet {
            reason: "isolated PG URL is not a postgres URL".into(),
            observed_at,
        },
        None => Observation::NotYet {
            reason: "no isolated PG URL".into(),
            observed_at,
        },
    };
    ResourceObservation {
        resource,
        observation,
    }
}

#[cfg(test)]
#[path = "adk_resources_tests.rs"]
mod tests;
