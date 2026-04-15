//! Platform-aware shell command execution.
//!
//! Abstracts `bash -c` (Unix) vs `cmd /C` (Windows) behind a unified API.

#![allow(dead_code)]

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

/// Resolve the AgentDesk git repository directory.
///
/// Priority: `AGENTDESK_REPO_DIR` env → scan all known roots for a git workspace → `~/AgentDesk`.
pub fn resolve_repo_dir() -> Option<String> {
    if let Ok(d) = std::env::var("AGENTDESK_REPO_DIR") {
        let trimmed = d.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }

    let is_git_dir = |p: &std::path::Path| p.join(".git").exists();

    // Try runtime_root/workspaces/agentdesk first (covers current deployment),
    // then the sibling root (dev ↔ release) so the dev server can find the
    // release repo where worktrees actually live.
    let mut candidates = Vec::new();
    if let Some(root) = crate::config::runtime_root() {
        candidates.push(root.join("workspaces").join("agentdesk"));
        // Sibling: if runtime_root is .adk/dev, also try .adk/release and vice versa
        if let Some(parent) = root.parent() {
            let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let sibling = if name == "dev" { "release" } else { "dev" };
            candidates.push(parent.join(sibling).join("workspaces").join("agentdesk"));
        }
    }
    for ws in &candidates {
        if is_git_dir(ws) {
            return Some(ws.to_string_lossy().into_owned());
        }
    }

    // Legacy fallback
    let legacy = dirs::home_dir().map(|h| h.join("AgentDesk"));
    if let Some(ref p) = legacy {
        if is_git_dir(p) {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    // Return legacy path even without .git — callers handle failures gracefully
    legacy.map(|p| p.to_string_lossy().into_owned())
}

fn expand_tilde(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        if path == "~" {
            return home.display().to_string();
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return home.join(rest).display().to_string();
        }
    }
    path.to_string()
}

pub(crate) fn looks_like_explicit_repo_path(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }

    if trimmed.starts_with('/')
        || trimmed.starts_with("~/")
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
    {
        return true;
    }

    let bytes = trimmed.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn load_repo_resolution_config() -> Option<(crate::config::Config, std::path::PathBuf)> {
    let explicit = std::env::var_os("AGENTDESK_CONFIG")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from);

    let mut candidates = Vec::new();
    if let Some(path) = explicit {
        candidates.push(path);
    }
    if let Some(root) = crate::config::runtime_root() {
        candidates.push(crate::runtime_layout::config_file_path(&root));
        candidates.push(crate::runtime_layout::legacy_config_file_path(&root));
    }

    for path in candidates {
        if !path.is_file() {
            continue;
        }
        if let Ok(config) = crate::config::load_from_path(&path) {
            let base_dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
            return Some((config, base_dir.to_path_buf()));
        }
    }

    None
}

fn configured_repo_dir(repo_id: &str) -> Option<String> {
    let (config, base_dir) = load_repo_resolution_config()?;
    let raw = config.github.repo_dirs.get(repo_id)?.trim();
    if raw.is_empty() {
        return None;
    }

    let expanded = expand_tilde(raw);
    let path = std::path::PathBuf::from(expanded);
    let resolved = if path.is_relative() {
        base_dir.join(path)
    } else {
        path
    };

    Some(
        std::fs::canonicalize(&resolved)
            .unwrap_or(resolved)
            .to_string_lossy()
            .into_owned(),
    )
}

pub(crate) fn parse_github_repo_from_remote(remote: &str) -> Option<String> {
    let trimmed = remote.trim().trim_end_matches(".git");
    let path = trimmed
        .strip_prefix("git@github.com:")
        .or_else(|| trimmed.strip_prefix("ssh://git@github.com/"))
        .or_else(|| trimmed.strip_prefix("https://github.com/"))
        .or_else(|| trimmed.strip_prefix("http://github.com/"))?
        .trim_matches('/');
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

fn repo_id_for_dir(repo_dir: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    parse_github_repo_from_remote(&String::from_utf8_lossy(&output.stdout))
}

fn ensure_git_worktree(path: &str) -> Result<(), String> {
    let output = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(path)
        .output()
        .map_err(|e| format!("git rev-parse failed for '{}': {e}", path))?;
    if !output.status.success() {
        return Err(format!("'{}' is not a git worktree", path));
    }
    Ok(())
}

/// Resolve the local git directory for a specific GitHub repo id.
///
/// Resolution order:
/// 1. `github.repo_dirs[repo_id]` from runtime config
/// 2. the current default repo iff its `origin` remote matches `repo_id`
///
/// When `repo_id` is absent, this simply mirrors `resolve_repo_dir()`.
/// When `repo_id` is present but no mapping exists, this returns an error
/// instead of silently falling back to the default repo.
pub fn resolve_repo_dir_for_id(repo_id: Option<&str>) -> Result<Option<String>, String> {
    let requested = repo_id.map(str::trim).filter(|value| !value.is_empty());
    let Some(requested) = requested else {
        return Ok(resolve_repo_dir());
    };

    if let Some(mapped_dir) = configured_repo_dir(requested) {
        ensure_git_worktree(&mapped_dir)?;
        if let Some(actual_repo_id) = repo_id_for_dir(&mapped_dir) {
            if actual_repo_id != requested {
                return Err(format!(
                    "Configured repo dir '{}' resolves to '{}' instead of requested '{}'",
                    mapped_dir, actual_repo_id, requested
                ));
            }
        }
        return Ok(Some(mapped_dir));
    }

    if let Some(default_dir) = resolve_repo_dir() {
        if repo_id_for_dir(&default_dir).as_deref() == Some(requested) {
            return Ok(Some(default_dir));
        }
    }

    Err(format!(
        "No local repo mapping for '{}'; configure github.repo_dirs.{} in agentdesk config",
        requested, requested
    ))
}

pub fn resolve_repo_dir_for_target(target_repo: Option<&str>) -> Result<Option<String>, String> {
    let requested = target_repo.map(str::trim).filter(|value| !value.is_empty());
    let Some(requested) = requested else {
        return Ok(resolve_repo_dir());
    };

    if looks_like_explicit_repo_path(requested) {
        let expanded = expand_tilde(requested);
        let path = std::path::PathBuf::from(expanded);
        let resolved = if path.is_relative() {
            std::env::current_dir()
                .map_err(|e| format!("cannot resolve repo path '{}': {}", requested, e))?
                .join(path)
        } else {
            path
        };
        let canonical = std::fs::canonicalize(&resolved).unwrap_or(resolved);
        let canonical_str = canonical.to_string_lossy().into_owned();
        ensure_git_worktree(&canonical_str)?;
        return Ok(Some(canonical_str));
    }

    resolve_repo_dir_for_id(Some(requested))
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

/// List tracked paths with local modifications in a git repo/worktree.
///
/// Untracked files are ignored because they do not participate in commit
/// resolution until they are added.
pub fn git_tracked_change_paths(repo_dir: &str) -> Option<Vec<String>> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let paths = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_end();
            if trimmed.len() < 4 {
                return None;
            }
            let path = trimmed[3..]
                .rsplit_once(" -> ")
                .map(|(_, new_path)| new_path)
                .unwrap_or(&trimmed[3..])
                .trim();
            (!path.is_empty()).then(|| path.to_string())
        })
        .collect::<Vec<_>>();
    Some(paths)
}

/// Find the most recent commit whose subject matches `(#issue_number)`.
///
/// Searches the last 20 commits to avoid expensive log scans.  Returns `None`
/// when no matching commit is found or git is unavailable.
pub fn git_latest_commit_for_issue(repo_dir: &str, issue_number: i64) -> Option<String> {
    let pattern = format!("(#{})", issue_number);
    Command::new("git")
        .args(["log", "--format=%H %s", "-20"])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .find(|line| line.contains(&pattern))
                .and_then(|line| line.split_whitespace().next())
                .map(str::to_string)
        })
}

/// Find the best commit for a dispatch that started at `since_iso` (ISO-8601).
///
/// Strategy (most reliable first):
/// 1. If `issue_number` is set, find the newest commit **after** `since_iso`
///    whose subject contains `(#issue_number)`.
/// 2. Otherwise, find the newest commit after `since_iso` (any subject).
/// 3. If nothing was committed since `since_iso`, return `None` so the caller
///    can fall back to `git_head_commit`.
///
/// `since_iso` is inclusive (`--after`).  Searches at most 50 recent commits.
pub fn git_best_commit_for_dispatch(
    repo_dir: &str,
    since_iso: &str,
    issue_number: Option<i64>,
) -> Option<String> {
    let output = Command::new("git")
        .args(["log", "--format=%H %s", "--after", since_iso, "-50"])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }

    // 1) Issue-scoped match within the time window
    if let Some(issue_number) = issue_number {
        let pattern = format!("(#{})", issue_number);
        if let Some(sha) = lines
            .iter()
            .find(|line| line.contains(&pattern))
            .and_then(|line| line.split_whitespace().next())
        {
            return Some(sha.to_string());
        }
    }

    // 2) Newest commit in the time window (first line = most recent)
    lines
        .first()
        .and_then(|line| line.split_whitespace().next())
        .map(str::to_string)
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

pub(crate) fn is_mainlike_branch(branch: &str) -> bool {
    matches!(branch, "main" | "master" | "origin/main" | "origin/master")
}

fn git_ref_ahead_count_from_commit(dir: &str, commit: &str, git_ref: &str) -> Option<u64> {
    let range = format!("{commit}..{git_ref}");
    Command::new("git")
        .args(["rev-list", "--count", &range])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u64>()
                .ok()
        })
}

/// Resolve a branch that actually contains `commit`, instead of assuming the
/// repo's current checkout branch is the reviewed branch.
///
/// Ranking rules:
/// 1. Branches whose tip is closest to the commit (`commit..branch` shortest)
/// 2. Branches whose name contains `preferred_substring`
/// 3. The caller-provided `preferred_branch`
/// 4. Local branches over `origin/*`
/// 5. Non-main branches over `main`/`master`
pub fn git_branch_containing_commit(
    dir: &str,
    commit: &str,
    preferred_branch: Option<&str>,
    preferred_substring: Option<&str>,
) -> Option<String> {
    let started = std::time::Instant::now();
    let output = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname:short)",
            "--contains",
            commit,
            "refs/heads",
            "refs/remotes/origin",
        ])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let mut seen = std::collections::HashSet::new();
    let candidates = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|branch| !branch.is_empty() && *branch != "HEAD" && *branch != "origin/HEAD")
        .filter(|branch| seen.insert((*branch).to_string()))
        .filter_map(|branch| {
            let ahead_count = git_ref_ahead_count_from_commit(dir, commit, branch)?;
            let preferred_name_match = preferred_substring
                .filter(|needle| !needle.is_empty())
                .is_some_and(|needle| branch.contains(needle));
            let preferred_branch_match = preferred_branch == Some(branch);
            Some((
                branch.to_string(),
                ahead_count,
                preferred_name_match,
                preferred_branch_match,
                branch.starts_with("origin/"),
                is_mainlike_branch(branch),
            ))
        })
        .collect::<Vec<_>>();
    let elapsed = started.elapsed();
    if candidates.len() > 20 || elapsed > std::time::Duration::from_millis(250) {
        tracing::warn!(
            "[platform::shell] git_branch_containing_commit scanned {} candidate branches for commit {} in {}ms (dir: {})",
            candidates.len(),
            &commit[..8.min(commit.len())],
            elapsed.as_millis(),
            dir
        );
    }

    candidates
        .into_iter()
        .min_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| b.3.cmp(&a.3))
                .then_with(|| a.4.cmp(&b.4))
                .then_with(|| a.5.cmp(&b.5))
                .then_with(|| a.0.cmp(&b.0))
        })
        .map(|(branch, _, _, _, _, _)| branch)
}

/// Resolve the merge-base SHA between two refs in a git directory.
pub fn git_merge_base(dir: &str, base_ref: &str, other_ref: &str) -> Option<String> {
    Command::new("git")
        .args(["merge-base", base_ref, other_ref])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Worktree info: (path, branch, commit).
#[derive(Clone)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
    pub commit: String,
}

/// Determine the upstream base ref for commit range comparisons.
///
/// Prefers `origin/main` over local `main` to avoid false negatives when
/// local main has already fast-forwarded past the worktree's commits.
fn upstream_base_ref(repo_dir: &str) -> String {
    let check = Command::new("git")
        .args(["rev-parse", "--verify", "origin/main"])
        .current_dir(repo_dir)
        .output();
    if let Ok(out) = check {
        if out.status.success() {
            return "origin/main".to_string();
        }
    }
    "main".to_string()
}

/// Find an active git worktree whose recent commits reference the given issue number.
///
/// Scans `git worktree list --porcelain`, then checks each non-main worktree for
/// commits mentioning `#<issue_number>` not reachable from the upstream base ref.
///
/// Uses `origin/main` (not local `main`) as the base ref so that worktrees remain
/// discoverable even after local main fast-forwards past their commits.
///
/// When multiple worktrees match, disambiguates by:
/// 1. Preferring branches whose name contains the issue number
/// 2. Among ties, preferring the worktree with the newest HEAD commit
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
    if !wt_path.is_empty() && wt_branch != "main" && wt_branch != "master" && !wt_branch.is_empty()
    {
        candidates.push((wt_path, wt_branch, wt_head));
    }

    // Use origin/main as base ref to avoid false negatives when local main
    // has already fast-forwarded past the worktree's commits.
    let base_ref = upstream_base_ref(repo_dir);
    let needle = format!("#{}", issue_number);
    let mut matches: Vec<WorktreeInfo> = Vec::new();

    for (path, branch, head) in &candidates {
        let check = Command::new("git")
            .args([
                "-C",
                path,
                "log",
                "--oneline",
                "--grep",
                &needle,
                &format!("{}..{}", base_ref, branch),
            ])
            .output()
            .ok();
        if let Some(out) = check {
            if out.status.success() {
                let log = String::from_utf8_lossy(&out.stdout);
                if !log.trim().is_empty() {
                    matches.push(WorktreeInfo {
                        path: path.clone(),
                        branch: branch.clone(),
                        commit: head.clone(),
                    });
                }
            }
        }
    }

    if matches.len() <= 1 {
        return matches.into_iter().next();
    }

    // Multiple matches: disambiguate.
    // Step 1: prefer branches whose name contains the issue number.
    let issue_str = issue_number.to_string();
    let name_hits: Vec<usize> = matches
        .iter()
        .enumerate()
        .filter(|(_, m)| m.branch.contains(&issue_str))
        .map(|(i, _)| i)
        .collect();

    let finalists: Vec<usize> = if name_hits.len() == 1 {
        return Some(matches.swap_remove(name_hits[0]));
    } else if !name_hits.is_empty() {
        name_hits
    } else {
        (0..matches.len()).collect()
    };

    // Step 2: among finalists, pick the one with the newest HEAD commit.
    let mut best_idx = finalists[0];
    let mut best_ts: i64 = 0;
    for &idx in &finalists {
        let ts = Command::new("git")
            .args(["log", "-1", "--format=%ct", &matches[idx].commit])
            .current_dir(repo_dir)
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<i64>()
                    .ok()
            })
            .unwrap_or(0);
        if ts > best_ts {
            best_ts = ts;
            best_idx = idx;
        }
    }
    Some(matches[best_idx].clone())
}

/// Find the newest mainline commit whose subject references the given issue number.
///
/// Used as a recovery fallback when a historical dispatch result omitted the
/// concrete `completed_commit` that review should inspect.
pub fn find_latest_commit_for_issue(repo_dir: &str, issue_number: i64) -> Option<String> {
    let pattern = format!(r"\(#{}\)", issue_number);
    let base_ref = upstream_base_ref(repo_dir);

    for args in [
        vec![
            "log".to_string(),
            "--format=%H".to_string(),
            "--perl-regexp".to_string(),
            "--grep".to_string(),
            pattern.clone(),
            "-n".to_string(),
            "1".to_string(),
            base_ref.clone(),
        ],
        vec![
            "log".to_string(),
            "--format=%H".to_string(),
            "--perl-regexp".to_string(),
            "--grep".to_string(),
            pattern.clone(),
            "--all".to_string(),
            "-n".to_string(),
            "1".to_string(),
        ],
    ] {
        let output = Command::new("git")
            .args(args.iter().map(String::as_str))
            .current_dir(repo_dir)
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !commit.is_empty() {
            return Some(commit);
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

    #[test]
    fn resolve_repo_dir_returns_some() {
        // In CI/dev environments, should always resolve to *something*
        let dir = resolve_repo_dir();
        assert!(dir.is_some(), "resolve_repo_dir should return Some");
    }

    #[test]
    fn resolve_repo_dir_env_override() {
        // When AGENTDESK_REPO_DIR is set, it takes priority
        unsafe { std::env::set_var("AGENTDESK_REPO_DIR", "/tmp/fake-repo") };
        let dir = resolve_repo_dir();
        unsafe { std::env::remove_var("AGENTDESK_REPO_DIR") };
        assert_eq!(dir, Some("/tmp/fake-repo".to_string()));
    }

    /// Helper: create a temporary git repo with an "origin" remote for testing.
    /// Returns (repo_dir, origin_dir) as temp dirs that are cleaned up on drop.
    fn setup_test_repo() -> (tempfile::TempDir, tempfile::TempDir) {
        let origin = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();

        // Init bare origin
        Command::new("git")
            .args(["init", "--bare"])
            .current_dir(origin.path())
            .output()
            .unwrap();

        // Init non-bare repo with explicit main branch and add origin
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        // Set git identity (CI environments may not have global user config)
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["remote", "add", "origin", origin.path().to_str().unwrap()])
            .current_dir(repo.path())
            .output()
            .unwrap();

        // Create initial commit on main and push to establish origin/main
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "initial"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        let push = Command::new("git")
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

    #[test]
    fn find_worktree_for_issue_uses_origin_main() {
        // Regression: when local main has already merged the issue commit,
        // the function should still find the worktree via origin/main base ref.
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        // Create a worktree branch with a commit referencing #42
        let wt_dir = repo.path().join("wt-42");
        Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "wt/fix-42",
                wt_dir.to_str().unwrap(),
            ])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "fix: something (#42)"])
            .current_dir(wt_dir.to_str().unwrap())
            .output()
            .unwrap();

        // Merge the worktree branch into local main (simulating fast-forward)
        Command::new("git")
            .args(["merge", "wt/fix-42"])
            .current_dir(repo_dir)
            .output()
            .unwrap();

        // Do NOT push to origin — origin/main is still behind.
        // With the old code (main..branch), this would return None
        // because the commit is reachable from local main.
        let result = find_worktree_for_issue(repo_dir, 42);
        assert!(
            result.is_some(),
            "Should find worktree even when local main has the commit (origin/main hasn't)"
        );
        let info = result.unwrap();
        assert_eq!(info.branch, "wt/fix-42");

        // Cleanup worktree
        Command::new("git")
            .args(["worktree", "remove", "--force", wt_dir.to_str().unwrap()])
            .current_dir(repo_dir)
            .output()
            .ok();
    }

    #[test]
    fn find_worktree_for_issue_disambiguates_multiple() {
        // Regression: when multiple worktrees have commits for the same issue,
        // prefer the branch whose name contains the issue number.
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        // Create two worktrees, both with commits mentioning #99
        let wt1_dir = repo.path().join("wt-generic");
        Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "wt/generic-branch",
                wt1_dir.to_str().unwrap(),
            ])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "chore: related to #99"])
            .current_dir(wt1_dir.to_str().unwrap())
            .output()
            .unwrap();

        let wt2_dir = repo.path().join("wt-99");
        Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "wt/fix-99",
                wt2_dir.to_str().unwrap(),
            ])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "fix: the real fix (#99)"])
            .current_dir(wt2_dir.to_str().unwrap())
            .output()
            .unwrap();

        let result = find_worktree_for_issue(repo_dir, 99);
        assert!(result.is_some(), "Should find a worktree for issue #99");
        let info = result.unwrap();
        assert_eq!(
            info.branch, "wt/fix-99",
            "Should prefer the branch whose name contains '99'"
        );

        // Cleanup worktrees
        for d in [&wt1_dir, &wt2_dir] {
            Command::new("git")
                .args(["worktree", "remove", "--force", d.to_str().unwrap()])
                .current_dir(repo_dir)
                .output()
                .ok();
        }
    }

    #[test]
    fn find_worktree_for_issue_newest_when_no_name_match() {
        // When no branch name contains the issue number, pick the most recent.
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        let wt1_dir = repo.path().join("wt-old");
        Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "wt/old-branch",
                wt1_dir.to_str().unwrap(),
            ])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        // Older commit with backdated author date
        Command::new("git")
            .args([
                "commit",
                "--allow-empty",
                "-m",
                "old work on #77",
                "--date",
                "2020-01-01T00:00:00",
            ])
            .current_dir(wt1_dir.to_str().unwrap())
            .output()
            .unwrap();

        let wt2_dir = repo.path().join("wt-new");
        Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "wt/new-branch",
                wt2_dir.to_str().unwrap(),
            ])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        // Newer commit (current date)
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "recent work on #77"])
            .current_dir(wt2_dir.to_str().unwrap())
            .output()
            .unwrap();

        let result = find_worktree_for_issue(repo_dir, 77);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(
            info.branch, "wt/new-branch",
            "Should prefer the worktree with the newest HEAD commit"
        );

        for d in [&wt1_dir, &wt2_dir] {
            Command::new("git")
                .args(["worktree", "remove", "--force", d.to_str().unwrap()])
                .current_dir(repo_dir)
                .output()
                .ok();
        }
    }

    #[test]
    fn find_latest_commit_for_issue_prefers_mainline_commit() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "fix: target commit (#269)"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        let expected = git_head_commit(repo_dir).unwrap();

        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "chore: unrelated"])
            .current_dir(repo_dir)
            .output()
            .unwrap();

        let found = find_latest_commit_for_issue(repo_dir, 269).unwrap();
        assert_eq!(found, expected);
    }

    #[test]
    fn git_merge_base_returns_branch_fork_point_when_main_has_advanced() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let fork_point = git_head_commit(repo_dir).unwrap();

        let wt_dir = repo.path().join("wt-542");
        let wt_path = wt_dir.to_str().unwrap();
        Command::new("git")
            .args(["worktree", "add", "-b", "wt/fix-542", wt_path])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "fix: branch-only change"])
            .current_dir(wt_path)
            .output()
            .unwrap();
        let branch_commit = git_head_commit(wt_path).unwrap();

        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "chore: main advanced"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        let main_commit = git_head_commit(repo_dir).unwrap();

        let merge_base = git_merge_base(repo_dir, "main", "wt/fix-542").unwrap();
        assert_eq!(merge_base, fork_point);
        assert_ne!(merge_base, branch_commit);
        assert_ne!(merge_base, main_commit);
    }

    #[test]
    fn git_branch_containing_commit_prefers_issue_branch_over_checked_out_main() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        Command::new("git")
            .args(["checkout", "-b", "feat/610-review"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "fix: review target (#610)"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        let reviewed_commit = git_head_commit(repo_dir).unwrap();

        Command::new("git")
            .args(["checkout", "main"])
            .current_dir(repo_dir)
            .output()
            .unwrap();

        let branch = git_branch_containing_commit(
            repo_dir,
            &reviewed_commit,
            git_branch_name(repo_dir).as_deref(),
            Some("610"),
        )
        .expect("branch containing reviewed commit must be found");

        assert_eq!(branch, "feat/610-review");
    }

    #[test]
    fn git_tracked_change_paths_returns_empty_for_clean_repo() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        let paths = git_tracked_change_paths(repo_dir).unwrap();
        assert!(paths.is_empty());
    }

    #[test]
    fn git_tracked_change_paths_ignores_untracked_and_reports_modified_files() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let tracked = repo.path().join("tracked.txt");
        let untracked = repo.path().join("untracked.txt");

        std::fs::write(&tracked, "v1\n").unwrap();
        Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add tracked fixture"])
            .current_dir(repo_dir)
            .output()
            .unwrap();

        std::fs::write(&tracked, "v2\n").unwrap();
        std::fs::write(&untracked, "scratch\n").unwrap();

        let paths = git_tracked_change_paths(repo_dir).unwrap();
        assert_eq!(paths, vec!["tracked.txt".to_string()]);
    }
}
