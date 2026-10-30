use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::definition::valid_id;
use super::model::Repository;
use super::profile::{Candidate, Checkout, ContractError, VerificationCheckoutKind, Workspace};
use super::verification_run::BoundSubject;
use crate::services::git::GitCommand;

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

fn git(repo: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
    GitCommand::new()
        .repo(repo)
        .args(args)
        .run_text()
        .map(|text| text.trim().to_owned())
        .map_err(|error| WorkspaceError::Git(error.to_string()))
}

pub fn repository_identity(repo: &Path) -> Result<String, WorkspaceError> {
    let url = git(repo, &["remote", "get-url", "origin"])?;
    Ok(crate::services::git::parse_github_repo_from_remote(&url).unwrap_or(url))
}

pub fn observe_checkout(
    repository: Repository,
    host: &str,
    path: &Path,
    observed_at: DateTime<Utc>,
) -> Result<WorkspaceObservation, WorkspaceError> {
    let head_sha = git(path, &["rev-parse", "HEAD"])?;
    let status = git(path, &["status", "--porcelain", "--untracked-files=all"])?;
    Ok(WorkspaceObservation {
        repository,
        repository_identity: repository_identity(path)?,
        host: host.into(),
        path: path.to_string_lossy().into_owned(),
        head_sha,
        tracked_dirty: status.lines().any(|l| !l.starts_with("??")),
        untracked_dirty: status.lines().any(|l| l.starts_with("??")),
        observed_at,
    })
}

fn ensure_detached_worktree(repo: &Path, path: &Path, sha: &str) -> Result<(), WorkspaceError> {
    if !path.join(".git").exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| WorkspaceError::Git(error.to_string()))?;
        }
        let target = path.to_string_lossy().into_owned();
        git(repo, &["worktree", "add", "--detach", &target, sha])?;
    }
    let head = git(path, &["rev-parse", "HEAD"])?;
    if head != sha {
        return Err(WorkspaceError::WrongHead(head));
    }
    Ok(())
}

pub fn prepare_editing(
    repo: &Path,
    root: &Path,
    key: &AdkWorkspaceKey,
    base_sha: &str,
    host: &str,
    observed_at: DateTime<Utc>,
) -> Result<WorkspaceObservation, WorkspaceError> {
    key.validate()?;
    let path = key.editing_dir(root);
    ensure_detached_worktree(repo, &path, base_sha)?;
    observe_checkout(Repository::Adk, host, &path, observed_at)
}

// The verification checkout is pinned to the sealed output and must be clean and the same repository.
pub fn prepare_verification(
    repo: &Path,
    root: &Path,
    key: &AdkWorkspaceKey,
    subject: &BoundSubject,
    host: &str,
    execution_id: &str,
    observed_at: DateTime<Utc>,
) -> Result<WorkspaceObservation, WorkspaceError> {
    key.validate()?;
    let path = key.verification_dir(root, execution_id);
    ensure_detached_worktree(repo, &path, &subject.output_sha)?;
    let observation = observe_checkout(subject.repository, host, &path, observed_at)?;
    verification_checkout_current(subject, &observation, &repository_identity(repo)?)?;
    Ok(observation)
}

// Re-checked before every phase start; the editing checkout is deliberately not consulted.
pub fn verification_checkout_current(
    subject: &BoundSubject,
    observation: &WorkspaceObservation,
    canonical_identity: &str,
) -> Result<(), WorkspaceError> {
    if observation.repository_identity != canonical_identity
        || observation.repository != subject.repository
    {
        return Err(WorkspaceError::RepositoryMismatch(
            observation.repository_identity.clone(),
        ));
    }
    if observation.head_sha != subject.output_sha {
        return Err(WorkspaceError::WrongHead(observation.head_sha.clone()));
    }
    if !observation.clean() {
        return Err(WorkspaceError::Dirty);
    }
    Ok(())
}

// Seal-time workspace check: editing and verification both at the produced output, both clean.
pub fn seal_workspace(
    candidate: &Candidate,
    editing: &WorkspaceObservation,
    verification: &WorkspaceObservation,
) -> Result<Workspace, WorkspaceError> {
    if editing.repository_identity != verification.repository_identity {
        return Err(WorkspaceError::RepositoryMismatch(
            verification.repository_identity.clone(),
        ));
    }
    if !editing.clean() || !verification.clean() {
        return Err(WorkspaceError::Dirty);
    }
    let workspace = Workspace {
        candidate: candidate.clone(),
        editing: editing.checkout(),
        verification: verification.checkout(),
        verification_kind: VerificationCheckoutKind::Isolated,
    };
    workspace.validate(candidate)?;
    Ok(workspace)
}

#[cfg(test)]
#[path = "adk_workspace_tests.rs"]
mod tests;
