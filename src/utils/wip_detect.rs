//! #1099 — WIP (Work In Progress) uncommitted-file detector.
//!
//! Background: regression analysis showed a non-trivial slice of cause-of-regression
//! events came from uncommitted edits in an agent's worktree. A fix that lives only
//! on disk disappears the next time the worktree is reset, and the only artifact
//! left behind is a "this used to work" report with no commit to inspect.
//!
//! This module exposes [`check_wip_uncommitted_files`] which inspects a workspace
//! directory and returns a structured warning when there are staged, unstaged, or
//! untracked file changes. The intent is to call this from the turn-end lifecycle
//! so that the agent (and the operator reading the turn transcript) sees an
//! explicit reminder before the worktree state is lost.
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

impl WipWarning {
    /// Total number of dirty entries across categories.
    pub fn total(&self) -> usize {
        self.staged.len() + self.unstaged.len() + self.untracked.len()
    }

    /// Render a short, agent-facing summary suitable for a turn-end notice.
    pub fn summary(&self) -> String {
        format!(
            "WIP uncommitted in {}: {} staged, {} unstaged, {} untracked. \
             Commit or explicitly discard before ending the turn — see `정본 편집 경로` in _shared.prompt.md.",
            self.workspace.display(),
            self.staged.len(),
            self.unstaged.len(),
            self.untracked.len(),
        )
    }
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn init_repo(dir: &Path) {
        // Minimal repo with deterministic identity so commit() works without
        // depending on the host's global git config.
        run(dir, &["init", "-q", "-b", "main"]);
        run(dir, &["config", "user.email", "wip-test@example.com"]);
        run(dir, &["config", "user.name", "WIP Test"]);
        run(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn run(dir: &Path, args: &[&str]) {
        GitCommand::new()
            .repo(dir)
            .args(args.iter().copied())
            .run_output()
            .unwrap_or_else(|error| panic!("git {args:?} failed in {}: {error}", dir.display()));
    }

    #[test]
    fn returns_none_for_clean_workspace() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        // Empty repo with no commits is still "clean" wrt working-tree status.
        fs::write(tmp.path().join("seed.txt"), "seed").unwrap();
        run(tmp.path(), &["add", "seed.txt"]);
        run(tmp.path(), &["commit", "-q", "-m", "seed"]);

        let warning = check_wip_uncommitted_files(tmp.path());
        assert!(
            warning.is_none(),
            "clean workspace should not warn: {warning:?}"
        );
    }

    #[test]
    fn returns_some_for_untracked_file() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        fs::write(tmp.path().join("seed.txt"), "seed").unwrap();
        run(tmp.path(), &["add", "seed.txt"]);
        run(tmp.path(), &["commit", "-q", "-m", "seed"]);

        fs::write(tmp.path().join("draft.md"), "wip").unwrap();

        let warning = check_wip_uncommitted_files(tmp.path()).expect("untracked → warning");
        assert!(warning.staged.is_empty());
        assert!(warning.unstaged.is_empty());
        assert_eq!(warning.untracked, vec!["draft.md".to_string()]);
        assert_eq!(warning.total(), 1);
        assert!(warning.summary().contains("untracked"));
    }

    #[test]
    fn returns_some_for_staged_and_unstaged_changes() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        let path = tmp.path().join("file.txt");
        fs::write(&path, "v1").unwrap();
        run(tmp.path(), &["add", "file.txt"]);
        run(tmp.path(), &["commit", "-q", "-m", "v1"]);

        // Staged change: edit + add.
        fs::write(&path, "v2").unwrap();
        run(tmp.path(), &["add", "file.txt"]);
        // Then introduce a second unstaged change on top.
        fs::write(&path, "v3").unwrap();

        let warning = check_wip_uncommitted_files(tmp.path()).expect("dirty → warning");
        assert_eq!(warning.staged, vec!["file.txt".to_string()]);
        assert_eq!(warning.unstaged, vec!["file.txt".to_string()]);
        assert!(warning.untracked.is_empty());
        assert_eq!(warning.total(), 2);
    }

    #[test]
    fn returns_none_when_not_a_git_repo() {
        let tmp = TempDir::new().unwrap();
        // No `git init`. Helper should stay silent rather than panic.
        let warning = check_wip_uncommitted_files(tmp.path());
        assert!(warning.is_none());
    }

    #[test]
    fn parse_porcelain_handles_rename_with_trailer() {
        // Format: "R  newpath\0oldpath\0??  fresh.md\0"
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"R  newpath");
        bytes.push(0);
        bytes.extend_from_slice(b"oldpath");
        bytes.push(0);
        bytes.extend_from_slice(b"?? fresh.md");
        bytes.push(0);

        let warning =
            parse_porcelain_z(&bytes, Path::new("/tmp/ws")).expect("non-empty status → warning");
        assert_eq!(warning.staged, vec!["newpath".to_string()]);
        assert!(warning.unstaged.is_empty());
        assert_eq!(warning.untracked, vec!["fresh.md".to_string()]);
    }

    #[test]
    fn summary_includes_workspace_path() {
        let warn = WipWarning {
            workspace: PathBuf::from("/tmp/example"),
            staged: vec!["a".into()],
            unstaged: vec![],
            untracked: vec!["b".into()],
        };
        let s = warn.summary();
        assert!(s.contains("/tmp/example"));
        assert!(s.contains("1 staged"));
        assert!(s.contains("1 untracked"));
    }
}
