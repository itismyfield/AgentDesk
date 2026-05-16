use regex::Regex;
use std::collections::{BTreeSet, HashSet};
use std::sync::OnceLock;

use super::git_command;

/// Get the current HEAD commit hash from a git repo directory.
///
/// Returns `None` if git is unavailable or the directory is not a repo.
pub fn git_head_commit(repo_dir: &str) -> Option<String> {
    git_command()
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

pub(crate) fn git_commit_for_ref(repo_dir: &str, git_ref: &str) -> Option<String> {
    git_command()
        .args(["rev-parse", git_ref])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn git_ref_exists(repo_dir: &str, git_ref: &str) -> bool {
    git_command()
        .args(["rev-parse", "--verify", git_ref])
        .current_dir(repo_dir)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn mainline_ref_for_range_search(repo_dir: &str) -> Option<&'static str> {
    ["main", "master", "origin/main", "origin/master"]
        .into_iter()
        .find(|candidate| git_ref_exists(repo_dir, candidate))
}

fn baseline_ref_for_dispatch(repo_dir: &str) -> Option<&'static str> {
    ["origin/main", "origin/master", "main", "master"]
        .into_iter()
        .find(|candidate| git_ref_exists(repo_dir, candidate))
}

pub fn git_dispatch_baseline_commit(repo_dir: &str) -> Option<String> {
    let baseline_ref = baseline_ref_for_dispatch(repo_dir)?;
    git_commit_for_ref(repo_dir, baseline_ref)
}

pub fn git_mainline_head_commit(repo_dir: &str) -> Option<String> {
    let main_ref = mainline_ref_for_range_search(repo_dir)?;
    git_commit_for_ref(repo_dir, main_ref)
}

pub fn git_mainline_commit_for_issue_since(
    repo_dir: &str,
    baseline_commit: &str,
    issue_number: i64,
) -> Option<String> {
    let main_ref = mainline_ref_for_range_search(repo_dir)?;
    let revert_re = Regex::new(r"(?m)^This reverts commit ([0-9a-fA-F]{7,40})\.?$")
        .expect("valid revert regex");
    let range = format!("{baseline_commit}..{main_ref}");
    let output = git_command()
        .args(["log", "--format=%H%x1f%s%x1f%B%x1e", &range])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let log = String::from_utf8_lossy(&output.stdout);
    if log.trim().is_empty() {
        return None;
    }

    let issue_re = issue_number_matcher(issue_number)?;
    let mut reverted_commits = HashSet::new();
    let mut candidates = Vec::new();

    for entry in log.split('\x1e') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.splitn(3, '\x1f');
        let sha = parts.next()?.trim().to_string();
        let subject = parts.next().unwrap_or_default().trim().to_string();
        let body = parts.next().unwrap_or_default().to_string();

        for capture in revert_re.captures_iter(&body) {
            if let Some(reverted_sha) = capture.get(1) {
                reverted_commits.insert(reverted_sha.as_str().to_ascii_lowercase());
            }
        }

        if !issue_re.is_match(&subject) && !issue_re.is_match(&body) {
            continue;
        }
        if subject.starts_with("Revert ") || revert_re.is_match(&body) {
            continue;
        }
        candidates.push(sha);
    }

    candidates
        .into_iter()
        .find(|sha| !reverted_commits.contains(&sha.to_ascii_lowercase()))
}

/// List tracked paths with local modifications in a git repo/worktree.
///
/// Untracked files are ignored because they do not participate in commit
/// resolution until they are added.
///
/// **Fail-open semantics**: returns `None` if git is unavailable or the command
/// fails (index lock, permission denied, corrupt repo state). Callers that use
/// this for *informational* purposes (e.g. a tracked-change preview in the
/// completion-guard summary) can safely treat `None` as "unknown".
///
/// **Do NOT** use this for safety-critical dirty-state checks — an index lock
/// or permission error would silently look like a clean worktree. Use
/// [`git_tracked_change_paths_strict`] instead, which surfaces the git failure
/// so callers can fail-closed.
pub fn git_tracked_change_paths(repo_dir: &str) -> Option<Vec<String>> {
    git_tracked_change_paths_strict(repo_dir).ok()
}

/// Strict (fail-closed) variant of [`git_tracked_change_paths`].
///
/// Returns `Err(String)` when the git invocation could not produce a reliable
/// answer (executable missing, permission denied, non-zero exit, etc.). Use
/// this in safety-critical decisions where treating an opaque git failure as
/// "clean worktree" would silently bypass a dirty-state guard.
///
/// Issue #2254 (bonus, from #2253 round-2 review): the previous
/// `Option`-only API meant `clean_exact_review_worktree_path` and
/// `resolve_repo_head_fallback_target_pg` accepted any git-failure as "no
/// tracked changes" via `unwrap_or_default()`. This variant lets those sites
/// distinguish the two outcomes.
pub fn git_tracked_change_paths_strict(repo_dir: &str) -> Result<Vec<String>, String> {
    let output = git_command()
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(repo_dir)
        .output()
        .map_err(|err| format!("git status spawn failed for {repo_dir}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git status --porcelain exited with status {:?} in {repo_dir}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
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
    Ok(paths)
}

/// Find the most recent commit whose subject references `#issue_number`.
///
/// Searches the last 20 commits to avoid expensive log scans. Returns `None`
/// when no matching commit is found or git is unavailable.
pub fn git_latest_commit_for_issue(repo_dir: &str, issue_number: i64) -> Option<String> {
    let issue_re = issue_number_matcher(issue_number)?;
    git_command()
        .args(["log", "--format=%H %s", "-20"])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .find(|line| issue_re.is_match(line))
                .and_then(|line| line.split_whitespace().next())
                .map(str::to_string)
        })
}

/// Find the best commit for a dispatch that started at `since_iso` (ISO-8601).
///
/// Strategy (most reliable first):
/// 1. If `issue_number` is set, find the newest commit **after** `since_iso`
///    whose subject references `#issue_number`.
/// 2. Otherwise, find the newest commit after `since_iso` (any subject).
/// 3. If nothing was committed since `since_iso`, return `None` so the caller
///    can fall back to `git_head_commit`.
///
/// `since_iso` is inclusive (`--after`). Searches at most 50 recent commits.
pub fn git_best_commit_for_dispatch(
    repo_dir: &str,
    since_iso: &str,
    issue_number: Option<i64>,
) -> Option<String> {
    let output = git_command()
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

    if let Some(issue_number) = issue_number {
        let issue_re = issue_number_matcher(issue_number)?;
        if let Some(sha) = lines
            .iter()
            .find(|line| issue_re.is_match(line))
            .and_then(|line| line.split_whitespace().next())
        {
            return Some(sha.to_string());
        }
    }

    lines
        .first()
        .and_then(|line| line.split_whitespace().next())
        .map(str::to_string)
}

pub(crate) fn issue_number_matcher(issue_number: i64) -> Option<Regex> {
    Regex::new(&format!(r"#{}\b", issue_number)).ok()
}

pub(crate) fn upstream_base_ref(repo_dir: &str) -> String {
    let check = git_command()
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
        let output = git_command()
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

/// Collect issue numbers referenced by commit subjects on the local `main`
/// branch, falling back to `master` if `main` does not exist.
pub fn git_mainline_issue_numbers(repo_dir: &str) -> Result<Vec<i64>, String> {
    static ISSUE_RE: OnceLock<Regex> = OnceLock::new();
    let regex = ISSUE_RE.get_or_init(|| Regex::new(r"#(\d+)").expect("valid issue regex"));

    let mut last_error: Option<String> = None;
    for branch in ["main", "master"] {
        let output = git_command()
            .args(["log", "--format=%s", "--first-parent", branch])
            .current_dir(repo_dir)
            .output()
            .map_err(|error| format!("git log {branch}: {error}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            last_error = Some(if stderr.is_empty() {
                format!("git log {branch} failed")
            } else {
                format!("git log {branch} failed: {stderr}")
            });
            continue;
        }

        let subjects = String::from_utf8_lossy(&output.stdout);
        let mut issues = BTreeSet::new();
        for capture in regex.captures_iter(&subjects) {
            if let Some(issue) = capture
                .get(1)
                .and_then(|matched| matched.as_str().parse::<i64>().ok())
            {
                issues.insert(issue);
            }
        }
        return Ok(issues.into_iter().collect());
    }

    Err(last_error.unwrap_or_else(|| "git log main failed".to_string()))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::services::git::test_support::setup_test_repo;

    #[test]
    fn git_dispatch_baseline_commit_prefers_origin_main() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        let local_commit = git_command()
            .args(["commit", "--allow-empty", "-m", "local only"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            local_commit.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&local_commit.stderr)
        );

        let baseline = git_dispatch_baseline_commit(repo_dir).expect("baseline commit");
        let origin_main = git_commit_for_ref(repo_dir, "origin/main").expect("origin/main");
        let local_main = git_commit_for_ref(repo_dir, "main").expect("main");

        assert_eq!(baseline, origin_main);
        assert_ne!(baseline, local_main);
    }

    #[test]
    fn git_mainline_commit_for_issue_since_skips_reverted_commits() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();
        let baseline = git_dispatch_baseline_commit(repo_dir).expect("baseline commit");

        std::fs::write(repo.path().join("direct-main.txt"), "mainline\n").unwrap();
        let add_output = git_command()
            .args(["add", "direct-main.txt"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            add_output.status.success(),
            "git add failed: {}",
            String::from_utf8_lossy(&add_output.stderr)
        );

        let issue_commit_output = git_command()
            .args(["commit", "-m", "#935 direct main attribution"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            issue_commit_output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&issue_commit_output.stderr)
        );
        let issue_commit = git_head_commit(repo_dir).expect("issue commit");

        assert_eq!(
            git_mainline_commit_for_issue_since(repo_dir, &baseline, 935),
            Some(issue_commit.clone())
        );

        let revert_output = git_command()
            .args(["revert", "--no-edit", &issue_commit])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            revert_output.status.success(),
            "git revert failed: {}",
            String::from_utf8_lossy(&revert_output.stderr)
        );

        assert_eq!(
            git_mainline_commit_for_issue_since(repo_dir, &baseline, 935),
            None
        );
    }

    #[test]
    fn worktree_issue_matchers_accept_bare_issue_subjects() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        git_command()
            .args(["commit", "--allow-empty", "-m", "#935 worktree attribution"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        let issue_commit = git_head_commit(repo_dir).expect("issue commit");

        git_command()
            .args([
                "commit",
                "--allow-empty",
                "-m",
                "chore: unrelated follow-up",
            ])
            .current_dir(repo_dir)
            .output()
            .unwrap();

        assert_eq!(
            git_best_commit_for_dispatch(repo_dir, "1970-01-01T00:00:00Z", Some(935)),
            Some(issue_commit.clone())
        );
        assert_eq!(
            git_latest_commit_for_issue(repo_dir, 935),
            Some(issue_commit)
        );
    }

    #[test]
    fn find_latest_commit_for_issue_prefers_mainline_commit() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        git_command()
            .args(["commit", "--allow-empty", "-m", "fix: target commit (#269)"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        let expected = git_head_commit(repo_dir).unwrap();

        git_command()
            .args(["commit", "--allow-empty", "-m", "chore: unrelated"])
            .current_dir(repo_dir)
            .output()
            .unwrap();

        let found = find_latest_commit_for_issue(repo_dir, 269).unwrap();
        assert_eq!(found, expected);
    }

    #[test]
    fn git_mainline_issue_numbers_deduplicates_subject_refs() {
        let (repo, _origin) = setup_test_repo();
        let repo_dir = repo.path().to_str().unwrap();

        git_command()
            .args([
                "commit",
                "--allow-empty",
                "-m",
                "fix: mainline merge (#404)",
            ])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        git_command()
            .args(["commit", "--allow-empty", "-m", "chore: unrelated"])
            .current_dir(repo_dir)
            .output()
            .unwrap();

        let issues = git_mainline_issue_numbers(repo_dir).unwrap();
        assert_eq!(issues, vec![404]);
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
        git_command()
            .args(["add", "tracked.txt"])
            .current_dir(repo_dir)
            .output()
            .unwrap();
        git_command()
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

#[cfg(test)]
mod fail_closed_tests {
    use super::*;

    /// #2254 bonus: the strict variant must surface a non-repo directory as
    /// `Err`, not silently return an empty "clean" list (the old fail-open
    /// behavior of `git_tracked_change_paths`).
    #[test]
    fn git_tracked_change_paths_strict_fails_closed_on_non_repo_dir() {
        let not_a_repo = tempfile::tempdir().unwrap();
        let repo_dir = not_a_repo.path().to_str().unwrap();
        let result = git_tracked_change_paths_strict(repo_dir);
        assert!(
            result.is_err(),
            "non-repo dir must surface a git failure to the caller, got {result:?}"
        );
        // The fail-open wrapper still maps that to None for legacy info-only
        // callers (e.g. completion_guard's tracked-change summary).
        assert!(git_tracked_change_paths(repo_dir).is_none());
    }
}
