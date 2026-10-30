use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::definition::{DefinitionHashes, WORKFLOW_VERSION};
use super::model::Repository;
use super::verification_run::{Digest, digest_of};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceLocation {
    pub acceptance_id: String,
    pub line_start: u32,
    pub line_end: u32,
    pub excerpt: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueObservation {
    pub repository: Repository,
    pub repository_identity: String,
    pub issue_number: u64,
    pub issue_url: String,
    pub observed_at: DateTime<Utc>,
    pub issue_updated_at: Option<DateTime<Utc>>,
    pub body: String,
    pub acceptance: Vec<AcceptanceLocation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementsSnapshot {
    pub observation: IssueObservation,
    pub body_digest: Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPin {
    pub hashes: DefinitionHashes,
    pub requirements_snapshot_hash: Digest,
    pub resolved_profile_hash: Digest,
    pub workflow_version: String,
    pub verification_spec_hash: Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum PinError {
    #[error("issue observation lacks an update time or body")]
    IncompleteObservation,
    #[error("acceptance location outside the body: {0}")]
    LocationOutOfBody(String),
    #[error("stored pin differs from current inputs: {0}")]
    Drift(String),
    #[error("encoding failed")]
    Encoding,
}

impl RequirementsSnapshot {
    // A snapshot with no update time cannot prove the issue was unchanged; it is not pinnable.
    pub fn observe(observation: IssueObservation) -> Result<Self, PinError> {
        if observation.issue_updated_at.is_none() || observation.body.trim().is_empty() {
            return Err(PinError::IncompleteObservation);
        }
        let lines = observation.body.lines().count() as u32;
        for location in &observation.acceptance {
            if location.line_start == 0
                || location.line_end < location.line_start
                || location.line_end > lines
            {
                return Err(PinError::LocationOutOfBody(location.acceptance_id.clone()));
            }
        }
        Ok(Self {
            body_digest: blake3::hash(observation.body.as_bytes())
                .to_hex()
                .to_string(),
            observation,
        })
    }

    pub fn hash(&self) -> Result<Digest, PinError> {
        digest_of(&(
            &self.observation.repository_identity,
            self.observation.issue_number,
            &self.observation.issue_updated_at,
            &self.body_digest,
            &self.observation.acceptance,
        ))
        .map_err(|_| PinError::Encoding)
    }
}

impl ExecutionPin {
    pub fn pin(
        hashes: DefinitionHashes,
        snapshot: &RequirementsSnapshot,
        resolved_profile_hash: Digest,
        verification_spec_hash: Digest,
    ) -> Result<Self, PinError> {
        Ok(Self {
            hashes,
            requirements_snapshot_hash: snapshot.hash()?,
            resolved_profile_hash,
            workflow_version: WORKFLOW_VERSION.into(),
            verification_spec_hash,
        })
    }

    pub fn hash(&self) -> Result<Digest, PinError> {
        digest_of(self).map_err(|_| PinError::Encoding)
    }

    // Resume re-validates the stored pin; a changed issue, profile or spec is drift, not a quiet adoption.
    pub fn revalidate(
        &self,
        hashes: &DefinitionHashes,
        snapshot: &RequirementsSnapshot,
        resolved_profile_hash: &str,
        verification_spec_hash: &str,
    ) -> Result<(), PinError> {
        let drift = |field: &str| PinError::Drift(field.into());
        if &self.hashes != hashes {
            return Err(drift("definition"));
        }
        if self.requirements_snapshot_hash != snapshot.hash()? {
            return Err(drift("requirements"));
        }
        if self.resolved_profile_hash != resolved_profile_hash {
            return Err(drift("profile"));
        }
        if self.verification_spec_hash != verification_spec_hash {
            return Err(drift("spec"));
        }
        if self.workflow_version != WORKFLOW_VERSION {
            return Err(drift("workflow"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "execution_pin_tests.rs"]
mod tests;
