use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::Repository;
use super::profile::Checkout;

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
