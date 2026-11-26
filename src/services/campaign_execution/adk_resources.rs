use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::campaign::profile::{Observation, ResourceIdentity, ResourceKind};

use super::resource_admission::{ResourceAdmission, wait_for_headroom};
use crate::domain::campaign::verification_run::CommandSpec;

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

// Both paths are trusted host configuration, separate from a candidate's checkout and inputs.
#[derive(Clone, Debug)]
pub struct AdkResources {
    checkout: PathBuf,
    wait_script: PathBuf,
    cargo_shim: PathBuf,
}

impl AdkResources {
    pub fn new(checkout: &Path, pipeline: &Path) -> io::Result<Self> {
        if !checkout.is_absolute() || !pipeline.is_absolute() {
            return Err(io::Error::other("resource roots must be absolute"));
        }
        let scripts = pipeline.join("scripts");
        let cargo_shim = scripts.join("cargo-shim/cargo");
        if cargo_shim.to_str().is_none() {
            return Err(io::Error::other("cargo shim path is not UTF-8"));
        }
        Ok(Self {
            checkout: checkout.to_path_buf(),
            wait_script: scripts.join("wait_headroom.sh"),
            cargo_shim,
        })
    }

    pub(super) async fn admission(&self, wait: Duration) -> ResourceAdmission {
        if which::which(&self.cargo_shim).is_err() {
            return ResourceAdmission::Unavailable;
        }
        wait_for_headroom(&self.checkout, &self.wait_script, wait).await
    }

    // Set the explicit shim path before the command becomes part of the verified input digest.
    pub fn bind_cargo(&self, command: &mut CommandSpec) -> io::Result<()> {
        if command.program == "cargo" {
            command.program = self.cargo_shim.to_string_lossy().into_owned();
        }
        let program = Path::new(&command.program);
        if program
            .file_name()
            .is_some_and(|name| name == "cargo" || name == "cargo.exe")
        {
            self.require_cargo_shim(command)?;
        }
        Ok(())
    }

    pub(super) fn require_cargo_shim(&self, command: &CommandSpec) -> io::Result<()> {
        if Path::new(&command.program) != self.cargo_shim
            || command
                .env_identity
                .keys()
                .any(|key| key.starts_with("ADK_CARGO_") || key == "ADK_REAL_CARGO")
            || std::env::var_os("ADK_CARGO_SLOT_HELD").is_some()
        {
            return Err(io::Error::other(
                "phase cargo command must acquire the host shim slot",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "adk_resources_tests.rs"]
mod tests;
