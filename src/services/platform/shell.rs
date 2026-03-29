//! Platform-aware shell command execution.
//!
//! Abstracts `bash -c` (Unix) vs `cmd /C` (Windows) behind a unified API.

use std::process::{Command, Output};

/// Execute a shell command string using the platform's default shell.
///
/// - **Unix**: `bash -c "<cmd>"`
/// - **Windows**: `cmd.exe /C "<cmd>"`
pub fn shell_command(cmd: &str) -> Result<Output, String> {
    #[cfg(unix)]
    let result = Command::new("bash").args(["-c", cmd]).output();
    #[cfg(windows)]
    let result = Command::new("cmd.exe").args(["/C", cmd]).output();

    result.map_err(|e| format!("Failed to execute shell command: {}", e))
}

/// Async version of `shell_command`.
pub async fn async_shell_command(cmd: &str) -> Result<Output, String> {
    #[cfg(unix)]
    let result = tokio::process::Command::new("bash")
        .args(["-c", cmd])
        .output()
        .await;
    #[cfg(windows)]
    let result = tokio::process::Command::new("cmd.exe")
        .args(["/C", cmd])
        .output()
        .await;

    result.map_err(|e| format!("Failed to execute shell command: {}", e))
}

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

/// Build a tokio `Command` for the platform shell.
pub fn async_shell_command_builder(cmd: &str) -> tokio::process::Command {
    #[cfg(unix)]
    {
        let mut c = tokio::process::Command::new("bash");
        c.args(["-c", cmd]);
        c
    }
    #[cfg(windows)]
    {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.args(["/C", cmd]);
        c
    }
}

// ── Common shell utilities ─────────────────────────────────────────

/// Get the short hostname of the current machine.
///
/// Equivalent to `hostname -s` on Unix.  Falls back to "localhost" on failure.
pub fn hostname_short() -> String {
    Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "localhost".to_string())
}

/// Get the current HEAD commit hash from a git repo directory.
///
/// Returns `None` if git is unavailable or the directory is not a repo.
pub fn git_head_commit(repo_dir: &str) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Get the current branch name from a git directory (repo or worktree).
pub fn git_branch_name(dir: &str) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| s != "HEAD") // detached HEAD → None
}

/// Worktree info: (path, branch, commit).
pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
    pub commit: String,
}

/// Find an active git worktree whose recent commits reference the given issue number.
///
/// Scans `git worktree list --porcelain`, then checks each non-main worktree for
/// commits mentioning `#<issue_number>` that are not reachable from main.
pub fn find_worktree_for_issue(repo_dir: &str, issue_number: i64) -> Option<WorktreeInfo> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);

    // Parse porcelain: blocks separated by blank lines.
    // Each block has "worktree <path>", "HEAD <sha>", "branch refs/heads/<name>".
    let mut candidates: Vec<(String, String, String)> = Vec::new(); // (path, branch, head)
    let mut wt_path = String::new();
    let mut wt_branch = String::new();
    let mut wt_head = String::new();
    for line in text.lines() {
        if line.starts_with("worktree ") {
            wt_path = line["worktree ".len()..].to_string();
        } else if line.starts_with("HEAD ") {
            wt_head = line["HEAD ".len()..].to_string();
        } else if line.starts_with("branch ") {
            wt_branch = line["branch ".len()..]
                .strip_prefix("refs/heads/")
                .unwrap_or(&line["branch ".len()..])
                .to_string();
        } else if line.is_empty() && !wt_path.is_empty() {
            // Skip the main worktree (branch == "main" or "master")
            if wt_branch != "main" && wt_branch != "master" && !wt_branch.is_empty() {
                candidates.push((wt_path.clone(), wt_branch.clone(), wt_head.clone()));
            }
            wt_path.clear();
            wt_branch.clear();
            wt_head.clear();
        }
    }
    // Handle last block (porcelain may not end with blank line)
    if !wt_path.is_empty()
        && wt_branch != "main"
        && wt_branch != "master"
        && !wt_branch.is_empty()
    {
        candidates.push((wt_path, wt_branch, wt_head));
    }

    let needle = format!("#{}", issue_number);
    for (path, branch, head) in &candidates {
        // Check if this branch has commits not on main that mention the issue
        let check = Command::new("git")
            .args([
                "-C",
                path,
                "log",
                "--oneline",
                "--grep",
                &needle,
                &format!("main..{}", branch),
            ])
            .output()
            .ok();
        if let Some(out) = check {
            if out.status.success() {
                let log = String::from_utf8_lossy(&out.stdout);
                if !log.trim().is_empty() {
                    // Found a worktree with commits for this issue
                    return Some(WorktreeInfo {
                        path: path.clone(),
                        branch: branch.clone(),
                        commit: head.clone(),
                    });
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_command_echo_works() {
        #[cfg(unix)]
        let output = shell_command("echo hello").unwrap();
        #[cfg(windows)]
        let output = shell_command("echo hello").unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("hello"));
    }

    #[test]
    fn shell_command_builder_works() {
        #[cfg(unix)]
        let cmd_str = "echo test123";
        #[cfg(windows)]
        let cmd_str = "echo test123";

        let output = shell_command_builder(cmd_str).output().unwrap();
        assert!(output.status.success());
    }
}
