//! Platform-aware shell command execution.
//!
//! Abstracts `bash -c` (Unix) vs `cmd /C` (Windows) behind a unified API.

use std::process::Command;

#[allow(unused_imports)]
pub(crate) use crate::services::git::{
    ManagedWorktreeCleanup, cleanup_managed_worktree, ensure_worktree_for_issue,
    find_latest_commit_for_issue, find_worktree_for_issue, git_best_commit_for_dispatch,
    git_branch_containing_commit, git_branch_name, git_dispatch_baseline_commit, git_head_commit,
    git_latest_commit_for_issue, git_mainline_commit_for_issue_since, git_mainline_head_commit,
    git_mainline_issue_numbers, git_merge_base, git_tracked_change_paths,
    git_tracked_change_paths_strict, is_mainlike_branch, is_no_repo_mapping_error,
    parse_github_repo_from_remote, resolve_repo_dir, resolve_repo_dir_for_id,
    resolve_repo_dir_for_target,
};

/// Build a `Command` for the platform shell, ready for further customization.
///
/// Returns a `Command` set up as `bash -c <cmd>` (Unix) or `cmd.exe /C <cmd>` (Windows).
/// Caller can add `.stdin()`, `.stdout()`, `.current_dir()`, etc.
pub fn shell_command_builder(cmd: &str) -> Command {
    #[cfg(unix)]
    {
        let mut c = Command::new("bash");
        c.args(["-c", cmd]);
        c
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd.exe");
        c.args(["/C", cmd]);
        c
    }
}

/// Get the short hostname of the current machine.
///
/// Equivalent to `hostname -s` on Unix. Falls back to "localhost" on failure.
pub fn hostname_short() -> String {
    Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "localhost".to_string())
}
