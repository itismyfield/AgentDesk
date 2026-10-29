use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::definition::valid_id;
use super::model::Repository;
use super::profile::{Checkout, ContractError};

// Editing space identity: issue numbers are not part of it, so two nodes on one issue stay apart.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AdkWorkspaceKey {
    pub campaign_id: String,
    pub run_id: String,
    pub node_id: String,
    pub series: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceObservation {
    pub repository: Repository,
    pub repository_identity: String,
    pub host: String,
    pub path: String,
    pub head_sha: String,
    pub tracked_dirty: bool,
    pub untracked_dirty: bool,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    #[error("workspace key component invalid: {0}")]
    InvalidKey(String),
    #[error("git failed: {0}")]
    Git(String),
    #[error("checkout is not at the expected commit: {0}")]
    WrongHead(String),
    #[error("checkout has tracked or untracked changes")]
    Dirty,
    #[error("checkout belongs to a different repository: {0}")]
    RepositoryMismatch(String),
    #[error(transparent)]
    Contract(#[from] ContractError),
}

impl AdkWorkspaceKey {
    pub fn validate(&self) -> Result<(), WorkspaceError> {
        for (name, value) in [
            ("campaign", &self.campaign_id),
            ("run", &self.run_id),
            ("node", &self.node_id),
        ] {
            if !valid_id(value) {
                return Err(WorkspaceError::InvalidKey(name.into()));
            }
        }
        if self.series == 0 {
            return Err(WorkspaceError::InvalidKey("series".into()));
        }
        Ok(())
    }

    fn node_dir(&self, root: &Path) -> PathBuf {
        root.join("campaigns")
            .join(&self.campaign_id)
            .join(&self.run_id)
            .join(&self.node_id)
            .join(format!("s{}", self.series))
    }

    pub fn editing_dir(&self, root: &Path) -> PathBuf {
        self.node_dir(root).join("editing")
    }

    // Verification checkouts and targets are per execution, so two checks on one node never share.
    pub fn verification_dir(&self, root: &Path, execution_id: &str) -> PathBuf {
        self.node_dir(root).join("verify").join(execution_id)
    }

    pub fn target_dir(&self, root: &Path, execution_id: &str) -> PathBuf {
        self.node_dir(root).join("target").join(execution_id)
    }

    pub fn scratch_dir(&self, root: &Path, execution_id: &str) -> PathBuf {
        self.node_dir(root).join("scratch").join(execution_id)
    }
}

impl WorkspaceObservation {
    pub fn checkout(&self) -> Checkout {
        Checkout {
            host: self.host.clone(),
            path: self.path.clone(),
            observed_sha: self.head_sha.clone(),
            observed_at: self.observed_at,
        }
    }

    pub fn clean(&self) -> bool {
        !self.tracked_dirty && !self.untracked_dirty
    }
}

#[cfg(test)]
#[path = "adk_workspace_tests.rs"]
mod tests;
