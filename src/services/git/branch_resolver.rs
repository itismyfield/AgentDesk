use super::git_command;

/// Get the current branch name from a git directory (repo or worktree).
pub fn git_branch_name(dir: &str) -> Option<String> {
    git_command()
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| s != "HEAD")
}

pub(crate) fn is_mainlike_branch(branch: &str) -> bool {
    matches!(branch, "main" | "master" | "origin/main" | "origin/master")
}

fn git_ref_ahead_count_from_commit(dir: &str, commit: &str, git_ref: &str) -> Option<u64> {
    let range = format!("{commit}..{git_ref}");
    git_command()
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
    let output = git_command()
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
            "[services::git] git_branch_containing_commit scanned {} candidate branches for commit {} in {}ms (dir: {})",
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
    git_command()
        .args(["merge-base", base_ref, other_ref])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::git::commit_resolver::git_head_commit;
    use crate::services::git::test_support::setup_test_repo;

    #[test]
    fn git_merge_base_returns_branch_fork_point_when_main_has_advanced() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let fork_point = git_head_commit(repo_dir).unwrap();

        let wt_dir = repo.path().join("wt-542");
        let wt_path = wt_dir.to_str().unwrap();
        git_command()
            .args(["worktree", "add", "-b", "wt/fix-542", wt_path])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        git_command()
            .args(["commit", "--allow-empty", "-m", "fix: branch-only change"])
            .current_dir(wt_path)
            .output()
            .unwrap();
        let branch_commit = git_head_commit(wt_path).unwrap();

        git_command()
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

        git_command()
            .args(["checkout", "-b", "feat/610-review"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        git_command()
            .args(["commit", "--allow-empty", "-m", "fix: review target (#610)"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        let reviewed_commit = git_head_commit(repo_dir).unwrap();

        git_command()
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
}
