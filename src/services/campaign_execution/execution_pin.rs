use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::definition::DefinitionHashes;
use super::model::Repository;
use super::verification_run::Digest;

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

#[cfg(test)]
#[path = "execution_pin_tests.rs"]
mod tests;
