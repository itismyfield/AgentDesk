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
    git_tracked_change_paths_strict,
};
pub(crate) use remote::parse_github_repo_from_remote;
pub use repo_resolver::{resolve_repo_dir, resolve_repo_dir_for_id, resolve_repo_dir_for_target};
#[allow(unused_imports)]
pub use runner::{GitCommand, GitCommandError};
pub use worktree_resolver::{
    ManagedWorktreeCleanup, automation_branch_name, cleanup_managed_worktree,
    ensure_automation_worktree, ensure_worktree_for_issue, find_automation_worktree,
    find_worktree_for_issue, remove_automation_worktree,
};

pub(crate) fn git_command() -> std::process::Command {
    crate::services::platform::binary_resolver::git_command()
}
