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
