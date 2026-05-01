use super::commit_resolver::upstream_base_ref;
use super::git_command;
use std::path::{Path, PathBuf};

/// Worktree info: (path, branch, commit).
#[derive(Clone)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
    pub commit: String,
}

#[derive(Clone)]
pub struct EnsuredWorktreeInfo {
    pub path: String,
    pub branch: String,
    pub commit: String,
    pub created: bool,
}

#[derive(Default)]
pub struct ManagedWorktreeCleanup {
    pub removed: usize,
    pub skipped_dirty: usize,
    pub skipped_unmerged: usize,
    pub skipped_unmanaged: usize,
    pub failed: usize,
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
    let output = git_command()
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);

    let mut candidates: Vec<(String, String, String)> = Vec::new();
    let mut wt_path = String::new();
    let mut wt_branch = String::new();
    let mut wt_head = String::new();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            wt_path = path.to_string();
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            wt_head = head.to_string();
        } else if let Some(branch) = line.strip_prefix("branch ") {
            wt_branch = branch
                .strip_prefix("refs/heads/")
                .unwrap_or(branch)
                .to_string();
        } else if line.is_empty() && !wt_path.is_empty() {
            if wt_branch != "main" && wt_branch != "master" && !wt_branch.is_empty() {
                candidates.push((wt_path.clone(), wt_branch.clone(), wt_head.clone()));
            }
            wt_path.clear();
            wt_branch.clear();
            wt_head.clear();
        }
    }
    if !wt_path.is_empty() && wt_branch != "main" && wt_branch != "master" && !wt_branch.is_empty()
    {
        candidates.push((wt_path, wt_branch, wt_head));
    }

    let base_ref = upstream_base_ref(repo_dir);
    let needle = format!("#{issue_number}");
    let mut matches: Vec<WorktreeInfo> = Vec::new();

    for (path, branch, head) in &candidates {
        let check = git_command()
            .args([
                "-C",
                path,
                "log",
                "--oneline",
                "--grep",
                &needle,
                &format!("{base_ref}..{branch}"),
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

    let mut best_idx = finalists[0];
    let mut best_ts: i64 = 0;
    for &idx in &finalists {
        let ts = git_command()
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

fn sanitize_path_segment(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "repo".to_string()
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn managed_worktrees_root(repo_dir: &str) -> Option<PathBuf> {
    let repo_name = Path::new(repo_dir)
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_path_segment)
        .unwrap_or_else(|| "repo".to_string());
    crate::config::runtime_root().map(|root| root.join("worktrees").join(repo_name))
}

pub fn ensure_worktree_for_issue(
    repo_dir: &str,
    issue_number: i64,
) -> Result<EnsuredWorktreeInfo, String> {
    if let Some(existing) = find_worktree_for_issue(repo_dir, issue_number) {
        return Ok(EnsuredWorktreeInfo {
            path: existing.path,
            branch: existing.branch,
            commit: existing.commit,
            created: false,
        });
    }

    let root = managed_worktrees_root(repo_dir)
        .ok_or_else(|| "cannot resolve AgentDesk runtime root for managed worktree".to_string())?;
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("create managed worktree root '{}': {error}", root.display()))?;

    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S").to_string();
    let branch = format!("adk/auto/issue-{issue_number}-{timestamp}");
    let path = root.join(format!("issue-{issue_number}-{timestamp}"));
    let base_ref = upstream_base_ref(repo_dir);
    let output = git_command()
        .args([
            "worktree",
            "add",
            "-b",
            &branch,
            path.to_str()
                .ok_or_else(|| format!("managed worktree path is not UTF-8: {}", path.display()))?,
            &base_ref,
        ])
        .current_dir(repo_dir)
        .output()
        .map_err(|error| format!("git worktree add failed for issue #{issue_number}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git worktree add failed for issue #{}: {} {}",
            issue_number,
            String::from_utf8_lossy(&output.stderr).trim(),
            String::from_utf8_lossy(&output.stdout).trim()
        ));
    }

    let commit = git_command()
        .args(["rev-parse", "HEAD"])
        .current_dir(&path)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();

    Ok(EnsuredWorktreeInfo {
        path: path.to_string_lossy().into_owned(),
        branch,
        commit,
        created: true,
    })
}

fn is_managed_worktree_path(repo_dir: &str, worktree_path: &str) -> bool {
    let Some(root) = managed_worktrees_root(repo_dir) else {
        return false;
    };
    let canonical_root = std::fs::canonicalize(root).ok();
    let canonical_path = std::fs::canonicalize(worktree_path).ok();
    match (canonical_root, canonical_path) {
        (Some(root), Some(path)) => path.starts_with(root),
        _ => false,
    }
}

fn worktree_head_is_merged_to_mainline(repo_dir: &str, worktree_path: &str) -> Option<bool> {
    let head = git_command()
        .args(["rev-parse", "HEAD"])
        .current_dir(worktree_path)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|head| !head.is_empty())?;
    let base_ref = upstream_base_ref(repo_dir);
    let status = git_command()
        .args(["merge-base", "--is-ancestor", &head, &base_ref])
        .current_dir(repo_dir)
        .status()
        .ok()?;
    Some(status.success())
}

pub fn cleanup_managed_worktree(repo_dir: &str, worktree_path: &str) -> ManagedWorktreeCleanup {
    let mut cleanup = ManagedWorktreeCleanup::default();
    if !is_managed_worktree_path(repo_dir, worktree_path) {
        cleanup.skipped_unmanaged += 1;
        return cleanup;
    }
    if !Path::new(worktree_path).exists() {
        cleanup.skipped_unmanaged += 1;
        return cleanup;
    }
    let dirty = git_command()
        .args(["status", "--porcelain"])
        .current_dir(worktree_path)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !String::from_utf8_lossy(&output.stdout).trim().is_empty())
        .unwrap_or(true);
    if dirty {
        cleanup.skipped_dirty += 1;
        return cleanup;
    }
    if worktree_head_is_merged_to_mainline(repo_dir, worktree_path) != Some(true) {
        cleanup.skipped_unmerged += 1;
        return cleanup;
    }

    let removed = git_command()
        .args(["worktree", "remove", worktree_path])
        .current_dir(repo_dir)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if removed {
        let _ = git_command()
            .args(["worktree", "prune"])
            .current_dir(repo_dir)
            .output();
        cleanup.removed += 1;
    } else {
        cleanup.failed += 1;
    }
    cleanup
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::services::git::test_support::setup_test_repo;

    #[test]
    fn find_worktree_for_issue_uses_origin_main() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        let wt_dir = repo.path().join("wt-42");
        git_command()
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
        git_command()
            .args(["commit", "--allow-empty", "-m", "fix: something (#42)"])
            .current_dir(wt_dir.to_str().unwrap())
            .output()
            .unwrap();

        git_command()
            .args(["merge", "wt/fix-42"])
            .current_dir(repo_dir)
            .output()
            .unwrap();

        let result = find_worktree_for_issue(repo_dir, 42);
        assert!(
            result.is_some(),
            "Should find worktree even when local main has the commit"
        );
        let info = result.unwrap();
        assert_eq!(info.branch, "wt/fix-42");

        git_command()
            .args(["worktree", "remove", "--force", wt_dir.to_str().unwrap()])
            .current_dir(repo_dir)
            .output()
            .ok();
    }

    #[test]
    fn find_worktree_for_issue_disambiguates_multiple() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        let wt1_dir = repo.path().join("wt-generic");
        git_command()
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
        git_command()
            .args(["commit", "--allow-empty", "-m", "chore: related to #99"])
            .current_dir(wt1_dir.to_str().unwrap())
            .output()
            .unwrap();

        let wt2_dir = repo.path().join("wt-99");
        git_command()
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
        git_command()
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

        for d in [&wt1_dir, &wt2_dir] {
            git_command()
                .args(["worktree", "remove", "--force", d.to_str().unwrap()])
                .current_dir(repo_dir)
                .output()
                .ok();
        }
    }

    #[test]
    fn find_worktree_for_issue_newest_when_no_name_match() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        let wt1_dir = repo.path().join("wt-old");
        git_command()
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
        git_command()
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
        git_command()
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
        git_command()
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
            git_command()
                .args(["worktree", "remove", "--force", d.to_str().unwrap()])
                .current_dir(repo_dir)
                .output()
                .ok();
        }
    }
}
