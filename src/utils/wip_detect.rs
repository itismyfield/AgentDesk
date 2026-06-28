//! #1099 — WIP (Work In Progress) uncommitted-file detector.
//!
//! Background: regression analysis showed a non-trivial slice of cause-of-regression
//! events came from uncommitted edits in an agent's worktree. A fix that lives only
//! on disk disappears the next time the worktree is reset, and the only artifact
//! left behind is a "this used to work" report with no commit to inspect.
//!
//! This module exposes [`check_wip_uncommitted_files`] which inspects a workspace
//! directory and returns a structured warning when there are staged, unstaged, or
//! untracked file changes. The Discord turn-end warning facade calls this before
//! completion surfaces so that the agent (and the operator reading the turn
//! transcript) sees an explicit reminder before the worktree state is lost.
//!
//! The shared prompt section "정본 편집 경로 (Canonical Edit Path)" in
//! `~/ObsidianVault/RemoteVault/adk-config/agents/_shared.prompt.md` documents the
//! prompt-side rule. This file is the runtime-side check.

use std::path::{Path, PathBuf};

use crate::services::git::GitCommand;

/// Structured warning describing uncommitted state in a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WipWarning {
    /// Workspace root the warning applies to.
    pub workspace: PathBuf,
    /// Files staged for commit but not yet committed.
    pub staged: Vec<String>,
    /// Files modified but not staged.
    pub unstaged: Vec<String>,
    /// Files not yet tracked by git.
    pub untracked: Vec<String>,
}

/// Inspect `workspace` (a git worktree root) and return `Some(WipWarning)` when
/// there are uncommitted changes, `None` otherwise.
///
/// Returns `None` when:
/// - the workspace is clean,
/// - the workspace is not a git repository,
/// - or `git` is not on PATH.
///
/// Errors from `git` are treated as "no warning" rather than propagating: this
/// helper is meant to be a soft turn-end advisory, not a fatal check.
pub fn check_wip_uncommitted_files(workspace: &Path) -> Option<WipWarning> {
    let output = GitCommand::new()
        .repo(workspace)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .run_output()
        .ok()?;

    parse_porcelain_z(&output.stdout, workspace)
}

/// Parse `git status --porcelain=v1 -z` output. Pulled out for unit testing.
fn parse_porcelain_z(bytes: &[u8], workspace: &Path) -> Option<WipWarning> {
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();

    // Records are NUL-terminated. Rename/copy records (R/C) carry an extra
    // NUL-separated "old path" trailer — track that to skip it.
    let mut iter = bytes.split(|b| *b == 0).peekable();
    while let Some(record) = iter.next() {
        if record.is_empty() {
            continue;
        }
        if record.len() < 3 {
            // Malformed/short record — skip defensively.
            continue;
        }
        let xy = &record[..2];
        let path = String::from_utf8_lossy(&record[3..]).into_owned();

        // For rename / copy entries the next NUL-separated token is the
        // original path. Consume it so we don't mis-classify it as a file.
        if xy[0] == b'R' || xy[0] == b'C' {
            let _ = iter.next();
        }

        match (xy[0], xy[1]) {
            (b'?', b'?') => untracked.push(path),
            (b'!', b'!') => { /* ignored — explicitly excluded */ }
            (x, y) => {
                if x != b' ' && x != b'?' {
                    staged.push(path.clone());
                }
                if y != b' ' && y != b'?' {
                    unstaged.push(path);
                }
            }
        }
    }

    if staged.is_empty() && unstaged.is_empty() && untracked.is_empty() {
        return None;
    }

    Some(WipWarning {
        workspace: workspace.to_path_buf(),
        staged,
        unstaged,
        untracked,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use crate::services::git::GitCommand;

    use super::check_wip_uncommitted_files;

    fn git_available() -> bool {
        GitCommand::new().arg("--version").run_output().is_ok()
    }

    fn init_git_repo() -> Option<TempDir> {
        if !git_available() {
            return None;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        git(temp.path(), &["init"]);
        Some(temp)
    }

    fn git(repo: &Path, args: &[&str]) {
        GitCommand::new()
            .repo(repo)
            .args(args.iter().copied())
            .run_output()
            .unwrap_or_else(|error| panic!("git {args:?} failed: {error}"));
    }

    fn committed_repo() -> Option<TempDir> {
        let temp = init_git_repo()?;
        fs::write(temp.path().join("tracked.txt"), "base\n").expect("seed tracked file");
        git(temp.path(), &["add", "tracked.txt"]);
        git(
            temp.path(),
            &[
                "-c",
                "user.name=AgentDesk Test",
                "-c",
                "user.email=agentdesk-test@example.invalid",
                "commit",
                "-m",
                "initial",
            ],
        );
        Some(temp)
    }

    #[test]
    fn clean_repo_suppresses_warning() {
        let Some(temp) = committed_repo() else {
            return;
        };

        assert_eq!(check_wip_uncommitted_files(temp.path()), None);
    }

    #[test]
    fn staged_file_is_reported() {
        let Some(temp) = init_git_repo() else {
            return;
        };
        fs::write(temp.path().join("staged.txt"), "staged\n").expect("write staged file");
        git(temp.path(), &["add", "staged.txt"]);

        let warning = check_wip_uncommitted_files(temp.path()).expect("staged warning");

        assert_eq!(warning.staged, vec!["staged.txt"]);
        assert!(warning.unstaged.is_empty());
        assert!(warning.untracked.is_empty());
    }

    #[test]
    fn unstaged_file_is_reported() {
        let Some(temp) = committed_repo() else {
            return;
        };
        fs::write(temp.path().join("tracked.txt"), "modified\n").expect("modify tracked file");

        let warning = check_wip_uncommitted_files(temp.path()).expect("unstaged warning");

        assert!(warning.staged.is_empty());
        assert_eq!(warning.unstaged, vec!["tracked.txt"]);
        assert!(warning.untracked.is_empty());
    }

    #[test]
    fn untracked_file_is_reported() {
        let Some(temp) = init_git_repo() else {
            return;
        };
        fs::write(temp.path().join("untracked.txt"), "untracked\n").expect("write untracked");

        let warning = check_wip_uncommitted_files(temp.path()).expect("untracked warning");

        assert!(warning.staged.is_empty());
        assert!(warning.unstaged.is_empty());
        assert_eq!(warning.untracked, vec!["untracked.txt"]);
    }

    #[test]
    fn non_git_directory_suppresses_warning() {
        let temp = tempfile::tempdir().expect("tempdir");

        assert_eq!(check_wip_uncommitted_files(temp.path()), None);
    }

    #[test]
    fn combined_dirty_counts_are_reported() {
        let Some(temp) = committed_repo() else {
            return;
        };
        fs::write(temp.path().join("tracked.txt"), "modified\n").expect("modify tracked file");
        fs::write(temp.path().join("staged.txt"), "staged\n").expect("write staged file");
        fs::write(temp.path().join("untracked.txt"), "untracked\n").expect("write untracked");
        git(temp.path(), &["add", "staged.txt"]);

        let warning = check_wip_uncommitted_files(temp.path()).expect("combined warning");

        assert_eq!(warning.staged.len(), 1);
        assert_eq!(warning.unstaged.len(), 1);
        assert_eq!(warning.untracked.len(), 1);
        assert_eq!(
            warning.staged.len() + warning.unstaged.len() + warning.untracked.len(),
            3
        );
    }
}
