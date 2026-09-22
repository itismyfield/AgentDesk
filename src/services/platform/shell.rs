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
    let mut command = Command::new("hostname");
    #[cfg(unix)]
    command.arg("-s");
    command
        .output()
        .ok()
        .and_then(|output| parse_short_hostname(output.status.success(), &output.stdout))
        .unwrap_or_else(|| "localhost".to_string())
}

fn parse_short_hostname(success: bool, stdout: &[u8]) -> Option<String> {
    if !success {
        return None;
    }
    let hostname = std::str::from_utf8(stdout).ok()?.trim();
    if hostname.is_empty() || hostname.chars().any(char::is_whitespace) {
        return None;
    }
    hostname
        .split('.')
        .next()
        .filter(|short| !short.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod hostname_tests {
    use super::*;

    #[test]
    fn accepts_native_and_fully_qualified_hostnames() {
        assert_eq!(
            parse_short_hostname(true, b"worker-1\r\n").as_deref(),
            Some("worker-1")
        );
        assert_eq!(
            parse_short_hostname(true, b"mac-mini.local\n").as_deref(),
            Some("mac-mini")
        );
    }

    #[test]
    fn rejects_failed_empty_or_invalid_hostname_output() {
        for (success, output) in [
            (false, b"worker-1".as_slice()),
            (true, b" \r\n"),
            (true, b"usage: hostname"),
            (true, b".local"),
            (true, b"\xff"),
        ] {
            assert_eq!(parse_short_hostname(success, output), None);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_hostname_matches_computer_name() {
        let expected = std::env::var("COMPUTERNAME").expect("Windows defines COMPUTERNAME");
        assert!(hostname_short().eq_ignore_ascii_case(&expected));
    }
}
