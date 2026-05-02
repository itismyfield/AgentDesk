//! Git repository, branch, commit, and worktree helpers.

pub mod branch_resolver;
pub mod commit_resolver;
pub mod remote;
pub mod repo_resolver;
pub mod runner;
pub mod worktree_resolver;

pub(crate) use branch_resolver::is_mainlike_branch;
pub use branch_resolver::{git_branch_containing_commit, git_branch_name, git_merge_base};
pub use commit_resolver::{
    find_latest_commit_for_issue, git_best_commit_for_dispatch, git_dispatch_baseline_commit,
    git_head_commit, git_latest_commit_for_issue, git_mainline_commit_for_issue_since,
    git_mainline_head_commit, git_mainline_issue_numbers, git_tracked_change_paths,
};
pub(crate) use remote::parse_github_repo_from_remote;
pub use repo_resolver::{resolve_repo_dir, resolve_repo_dir_for_id, resolve_repo_dir_for_target};
#[allow(unused_imports)]
pub use runner::{GitCommand, GitCommandError};
pub use worktree_resolver::{
    ManagedWorktreeCleanup, cleanup_managed_worktree, ensure_worktree_for_issue,
    find_worktree_for_issue,
};

pub(crate) fn git_command() -> std::process::Command {
    crate::services::platform::binary_resolver::git_command()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) mod test_support {
    use tempfile::TempDir;

    use super::git_command;

    /// Create a temporary git repo with an `origin/main` baseline.
    pub(super) fn setup_test_repo() -> (TempDir, TempDir) {
        let origin = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();

        git_command()
            .args(["init", "--bare"])
            .current_dir(origin.path())
            .output()
            .unwrap();

        git_command()
            .args(["init", "-b", "main"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        git_command()
            .args(["config", "user.email", "test@test.com"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        git_command()
            .args(["config", "user.name", "Test"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        git_command()
            .args(["remote", "add", "origin", origin.path().to_str().unwrap()])
            .current_dir(repo.path())
            .output()
            .unwrap();

        git_command()
            .args(["commit", "--allow-empty", "-m", "initial"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        let push = git_command()
            .args(["push", "-u", "origin", "main"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            push.status.success(),
            "git push failed: {}",
            String::from_utf8_lossy(&push.stderr)
        );

        (repo, origin)
    }
}
