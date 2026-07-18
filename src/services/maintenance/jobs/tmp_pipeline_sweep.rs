//! `storage.tmp_pipeline_sweep` — daily cleanup of stale pipeline directories
//! directly under `/private/tmp`.
//!
//! The basename whitelist is a safety boundary: only `adk-*` and
//! `agentdesk-*` directories can become candidates. Unrelated `/private/tmp`
//! content, including orchestration scratchpads, never reaches age, owner, or
//! removal logic.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;

use super::worktree_orphan_sweep::{
    collect_live_tmux_pane_paths, has_live_tmux_owner, remove_orphan_worktree,
};

const ALLOWED_TMP_BASE: &str = "/private/tmp";
const APPROVED_NAME_PREFIXES: &[&str] = &["adk-", "agentdesk-"];
const DEFAULT_MIN_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const MINIMUM_MIN_AGE: Duration = Duration::from_secs(60 * 60);
const MAX_ACTIVITY_WALK_DEPTH: usize = 8;
const MAX_ACTIVITY_WALK_ENTRIES: usize = 1_024;

#[derive(Debug, Clone)]
pub struct Config {
    /// The only directory scanned; entries below its direct children are never
    /// enumerated as independent cleanup candidates.
    pub tmp_root: PathBuf,
    /// Approved pipeline-directory prefixes. Values outside the fixed safety
    /// whitelist are ignored by [`is_sweep_candidate`].
    pub name_prefixes: Vec<String>,
    /// Minimum inactive age before a candidate is eligible for removal.
    pub min_age: Duration,
    /// Identify removable candidates without deleting them.
    pub dry_run: bool,
}

impl Config {
    pub fn default_runtime() -> Self {
        Self {
            tmp_root: PathBuf::from("/private/tmp"),
            name_prefixes: APPROVED_NAME_PREFIXES
                .iter()
                .map(|prefix| (*prefix).to_string())
                .collect(),
            min_age: DEFAULT_MIN_AGE,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Whitelisted direct-child directories whose activity time was inspected.
    pub scanned: u64,
    pub keep_active: u64,
    pub keep_fresh: u64,
    /// Direct children rejected before candidate processing by the basename
    /// prefix hard gate.
    pub keep_non_matching: u64,
    pub removed: u64,
    pub errors: u64,
}

/// True only for an approved configured prefix. The fixed approved-prefix check
/// prevents a broad or empty config value from widening the deletion boundary.
pub(crate) fn is_sweep_candidate(basename: &str, prefixes: &[String]) -> bool {
    APPROVED_NAME_PREFIXES.iter().any(|approved| {
        prefixes.iter().any(|configured| configured == approved) && basename.starts_with(approved)
    })
}

/// True when `last_activity` is more than `min_age` before `now`. A future or
/// otherwise incomparable timestamp is treated as fresh so it cannot authorize
/// deletion.
pub(crate) fn is_stale(last_activity: SystemTime, now: SystemTime, min_age: Duration) -> bool {
    now.duration_since(last_activity)
        .map(|age| age > min_age)
        .unwrap_or(false)
}

/// True when a canonical root is the approved base itself or lies below it.
/// [`Path::starts_with`] compares path components, so `/private/tmpfoo` does not
/// match `/private/tmp`.
pub(crate) fn tmp_root_within_allowed_base(canonical_root: &Path, canonical_base: &Path) -> bool {
    canonical_root.starts_with(canonical_base)
}

fn effective_min_age(configured_min_age: Duration) -> Duration {
    configured_min_age.max(MINIMUM_MIN_AGE)
}

/// The pure deletion decision after candidate, age, and live-owner checks.
pub(crate) fn should_remove(candidate: bool, stale: bool, has_live_owner: bool) -> bool {
    candidate && stale && !has_live_owner
}

/// Sweep stale, unowned pipeline directories from the direct children of
/// `config.tmp_root`. A failed tmux query leaves every directory untouched.
pub async fn run(config: Config) -> Result<()> {
    let report = run_inner(&config).await?;
    tracing::info!(
        target: "maintenance",
        job = "storage.tmp_pipeline_sweep",
        tmp_root = %config.tmp_root.display(),
        scanned = report.scanned,
        keep_active = report.keep_active,
        keep_fresh = report.keep_fresh,
        keep_non_matching = report.keep_non_matching,
        removed = report.removed,
        errors = report.errors,
        dry_run = config.dry_run,
        "tmp_pipeline_sweep completed"
    );
    Ok(())
}

async fn run_inner(config: &Config) -> Result<SweepReport> {
    let mut report = SweepReport::default();

    let canonical_base = match std::fs::canonicalize(ALLOWED_TMP_BASE) {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(
                target: "maintenance",
                job = "storage.tmp_pipeline_sweep",
                allowed_tmp_base = ALLOWED_TMP_BASE,
                error = %error,
                "failed to canonicalize approved tmp pipeline sweep base; skipping"
            );
            report.errors = report.errors.saturating_add(1);
            return Ok(report);
        }
    };
    let canonical_root = match std::fs::canonicalize(&config.tmp_root) {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(
                target: "maintenance",
                job = "storage.tmp_pipeline_sweep",
                tmp_root = %config.tmp_root.display(),
                error = %error,
                "failed to canonicalize tmp pipeline sweep root; skipping"
            );
            report.errors = report.errors.saturating_add(1);
            return Ok(report);
        }
    };
    if !tmp_root_within_allowed_base(&canonical_root, &canonical_base) {
        tracing::warn!(
            target: "maintenance",
            job = "storage.tmp_pipeline_sweep",
            tmp_root = %config.tmp_root.display(),
            canonical_tmp_root = %canonical_root.display(),
            allowed_tmp_base = %canonical_base.display(),
            "tmp pipeline sweep root is outside the approved base; skipping"
        );
        return Ok(report);
    }

    // Fail closed before inspecting candidates: an unavailable tmux probe means
    // no deletion can prove that it lacks a live owner.
    let Some(live_tmux_paths) = collect_live_tmux_pane_paths() else {
        tracing::warn!(
            target: "maintenance",
            job = "storage.tmp_pipeline_sweep",
            tmp_root = %config.tmp_root.display(),
            "tmux query failed; skipping tmp pipeline sweep (fail-closed)"
        );
        return Ok(report);
    };

    let entries = match std::fs::read_dir(&canonical_root) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(
                target: "maintenance",
                job = "storage.tmp_pipeline_sweep",
                tmp_root = %canonical_root.display(),
                error = %error,
                "failed to read tmp pipeline sweep root"
            );
            report.errors = report.errors.saturating_add(1);
            return Ok(report);
        }
    };

    let now = SystemTime::now();
    let min_age = effective_min_age(config.min_age);
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(
                    target: "maintenance",
                    job = "storage.tmp_pipeline_sweep",
                    error = %error,
                    "failed to inspect tmp pipeline sweep entry"
                );
                report.errors = report.errors.saturating_add(1);
                continue;
            }
        };

        let basename = entry.file_name();
        let Some(basename) = basename.to_str() else {
            report.keep_non_matching = report.keep_non_matching.saturating_add(1);
            continue;
        };

        // This is intentionally the first candidate filter. Non-matching paths
        // never enter directory, age, ownership, or removal handling.
        if !is_sweep_candidate(basename, &config.name_prefixes) {
            report.keep_non_matching = report.keep_non_matching.saturating_add(1);
            continue;
        }

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                tracing::warn!(
                    target: "maintenance",
                    job = "storage.tmp_pipeline_sweep",
                    path = %entry.path().display(),
                    error = %error,
                    "failed to read tmp pipeline candidate type"
                );
                report.errors = report.errors.saturating_add(1);
                continue;
            }
        };
        // A symlink is not a direct-child directory for this job. This also
        // keeps the cleanup boundary from traversing to a target outside tmp.
        if !file_type.is_dir() {
            continue;
        }

        let dir = entry.path();
        report.scanned = report.scanned.saturating_add(1);

        let activity = match bounded_recursive_last_activity(&dir) {
            Ok(activity) => activity,
            Err(error) => {
                tracing::warn!(
                    target: "maintenance",
                    job = "storage.tmp_pipeline_sweep",
                    path = %dir.display(),
                    error = %error,
                    "failed to determine tmp pipeline candidate activity; keeping"
                );
                report.errors = report.errors.saturating_add(1);
                continue;
            }
        };
        if activity.traversal_errors > 0 {
            tracing::warn!(
                target: "maintenance",
                job = "storage.tmp_pipeline_sweep",
                path = %dir.display(),
                traversal_errors = activity.traversal_errors,
                "failed to inspect part of tmp pipeline candidate activity; keeping"
            );
            report.errors = report.errors.saturating_add(activity.traversal_errors);
        }
        let stale = is_stale(activity.last_activity, now, min_age);
        if !stale {
            report.keep_fresh = report.keep_fresh.saturating_add(1);
            continue;
        }

        let has_owner = has_live_tmux_owner(&dir, &live_tmux_paths);
        if !should_remove(true, stale, has_owner) {
            report.keep_active = report.keep_active.saturating_add(1);
            continue;
        }

        if config.dry_run {
            continue;
        }

        match remove_orphan_worktree(&dir).await {
            Ok(()) => {
                report.removed = report.removed.saturating_add(1);
            }
            Err(error) => {
                tracing::warn!(
                    target: "maintenance",
                    job = "storage.tmp_pipeline_sweep",
                    path = %dir.display(),
                    error = %error,
                    "failed to remove stale tmp pipeline directory"
                );
                report.errors = report.errors.saturating_add(1);
            }
        }
    }

    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActivityWalk {
    last_activity: SystemTime,
    traversal_errors: u64,
}

/// Identify recursion targets from the uncached lstat metadata result.
fn is_real_directory(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_dir()
}

/// Return the latest modification time in a bounded, non-symlink-following
/// walk of `dir`. Exceeding either bound returns a fresh timestamp so the
/// caller keeps the candidate rather than deleting it.
fn bounded_recursive_last_activity(dir: &Path) -> std::io::Result<ActivityWalk> {
    bounded_recursive_last_activity_with_limits(
        dir,
        MAX_ACTIVITY_WALK_DEPTH,
        MAX_ACTIVITY_WALK_ENTRIES,
    )
}

fn bounded_recursive_last_activity_with_limits(
    dir: &Path,
    max_depth: usize,
    max_entries: usize,
) -> std::io::Result<ActivityWalk> {
    let mut latest = std::fs::symlink_metadata(dir)?.modified()?;
    let mut pending = vec![(dir.to_path_buf(), 0usize)];
    let mut visited_entries = 0usize;
    let mut traversal_errors = 0u64;

    while let Some((current_dir, depth)) = pending.pop() {
        let entries = match std::fs::read_dir(&current_dir) {
            Ok(entries) => entries,
            Err(_) => {
                latest = SystemTime::now();
                traversal_errors = traversal_errors.saturating_add(1);
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    latest = SystemTime::now();
                    traversal_errors = traversal_errors.saturating_add(1);
                    continue;
                }
            };
            visited_entries = visited_entries.saturating_add(1);
            if visited_entries > max_entries {
                return Ok(ActivityWalk {
                    last_activity: SystemTime::now(),
                    traversal_errors,
                });
            }

            let metadata = match std::fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(_) => {
                    latest = SystemTime::now();
                    traversal_errors = traversal_errors.saturating_add(1);
                    continue;
                }
            };
            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(_) => {
                    latest = SystemTime::now();
                    traversal_errors = traversal_errors.saturating_add(1);
                    continue;
                }
            };
            if modified > latest {
                latest = modified;
            }

            // Recurse only when this lstat result identifies a real directory;
            // never use a cached DirEntry type for that decision. A path can
            // still change between lstat and read_dir, but the remaining window
            // is bounded and read-only; deletion remains candidate-root-only.
            if is_real_directory(&metadata) {
                if depth >= max_depth {
                    return Ok(ActivityWalk {
                        last_activity: SystemTime::now(),
                        traversal_errors,
                    });
                }
                pending.push((entry.path(), depth + 1));
            }
        }
    }

    Ok(ActivityWalk {
        last_activity: latest,
        traversal_errors,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use filetime::{FileTime, set_file_mtime};

    use super::{
        APPROVED_NAME_PREFIXES, MINIMUM_MIN_AGE, bounded_recursive_last_activity,
        bounded_recursive_last_activity_with_limits, effective_min_age, is_real_directory,
        is_stale, is_sweep_candidate, should_remove, tmp_root_within_allowed_base,
    };
    use crate::services::maintenance::jobs::worktree_orphan_sweep::has_live_tmux_owner;

    fn prefixes() -> Vec<String> {
        APPROVED_NAME_PREFIXES
            .iter()
            .map(|prefix| (*prefix).to_string())
            .collect()
    }

    #[test]
    fn basename_whitelist_excludes_non_pipeline_tmp_content() {
        let prefixes = prefixes();

        for name in [
            "claude-501",
            "__pycache__",
            "4254-mut-state",
            "CookingHeart-pr191-review",
        ] {
            assert!(
                !is_sweep_candidate(name, &prefixes),
                "non-pipeline tmp entry {name:?} must never be a deletion candidate"
            );
        }
        assert!(is_sweep_candidate("adk-impl-4173", &prefixes));
        assert!(is_sweep_candidate("agentdesk-pr123", &prefixes));
    }

    #[test]
    fn age_gate_requires_strictly_more_than_minimum_age() {
        let now = SystemTime::now();
        let minimum_age = Duration::from_secs(72 * 60 * 60);
        let fresh = now.checked_sub(Duration::from_secs(60 * 60)).unwrap();
        let exactly_minimum_age_old = now.checked_sub(minimum_age).unwrap();
        let stale = now.checked_sub(Duration::from_secs(73 * 60 * 60)).unwrap();

        assert!(!is_stale(fresh, now, minimum_age));
        assert!(!is_stale(exactly_minimum_age_old, now, minimum_age));
        assert!(is_stale(stale, now, minimum_age));
    }

    #[test]
    fn configured_minimum_age_cannot_drop_below_one_hour() {
        assert_eq!(effective_min_age(Duration::ZERO), MINIMUM_MIN_AGE);
        assert_eq!(
            effective_min_age(Duration::from_secs(2 * 60 * 60)),
            Duration::from_secs(2 * 60 * 60)
        );
    }

    #[test]
    fn canonical_tmp_root_must_stay_within_approved_base() {
        let base = Path::new("/private/tmp");

        assert!(tmp_root_within_allowed_base(base, base));
        assert!(tmp_root_within_allowed_base(
            Path::new("/private/tmp/adk-impl-4173"),
            base
        ));
        assert!(!tmp_root_within_allowed_base(Path::new("/var/tmp"), base));
        assert!(!tmp_root_within_allowed_base(Path::new("/"), base));
        assert!(!tmp_root_within_allowed_base(
            Path::new("/private/tmpfoo"),
            base
        ));
    }

    #[test]
    fn removal_decision_requires_candidate_staleness_and_no_live_owner() {
        assert!(!should_remove(false, true, false));
        assert!(!should_remove(true, false, false));
        assert!(should_remove(true, true, false));
        assert!(!should_remove(true, true, true));
    }

    #[test]
    fn live_tmux_owner_blocks_the_pure_removal_decision() {
        let candidate = Path::new("/private/tmp/adk-impl-4173");
        let mut live_tmux_paths = HashSet::new();
        live_tmux_paths.insert("/private/tmp/adk-impl-4173/src".to_string());

        let has_owner = has_live_tmux_owner(candidate, &live_tmux_paths);
        assert!(
            has_owner,
            "a nested live pane must own its candidate directory"
        );
        assert!(!should_remove(true, true, has_owner));
    }

    #[test]
    fn deep_recent_activity_keeps_an_old_candidate_fresh() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("adk-candidate");
        let nested = candidate.join("src/deep");
        fs::create_dir_all(&nested).unwrap();
        let active_file = nested.join("active.rs");
        fs::write(&active_file, "recent activity").unwrap();

        let now = SystemTime::now();
        let old = now.checked_sub(Duration::from_secs(2 * 60 * 60)).unwrap();
        let recent = now.checked_sub(Duration::from_secs(60)).unwrap();
        set_mtime(&candidate, old);
        set_mtime(candidate.join("src"), old);
        set_mtime(&nested, old);
        set_mtime(&active_file, recent);

        let activity = bounded_recursive_last_activity(&candidate).unwrap();

        assert!(
            !is_stale(activity.last_activity, now, Duration::from_secs(60 * 60)),
            "recent depth-two file activity must prevent stale removal even when directory mtimes are old"
        );
    }

    #[test]
    fn depth_limit_fails_closed_with_fresh_activity() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("adk-candidate");
        let child = candidate.join("child");
        fs::create_dir_all(&child).unwrap();

        let now = SystemTime::now();
        let old = now.checked_sub(Duration::from_secs(2 * 60 * 60)).unwrap();
        set_mtime(&candidate, old);
        set_mtime(&child, old);

        let activity = bounded_recursive_last_activity_with_limits(&candidate, 0, 10).unwrap();

        assert!(
            !is_stale(activity.last_activity, now, Duration::from_secs(60 * 60)),
            "reaching the depth limit must keep the candidate rather than authorize removal"
        );
    }

    #[test]
    fn entry_limit_fails_closed_with_fresh_activity() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("adk-candidate");
        fs::create_dir_all(&candidate).unwrap();
        fs::write(candidate.join("first"), "one").unwrap();
        fs::write(candidate.join("second"), "two").unwrap();

        let now = SystemTime::now();
        let old = now.checked_sub(Duration::from_secs(2 * 60 * 60)).unwrap();
        set_mtime(&candidate, old);
        for child in [candidate.join("first"), candidate.join("second")] {
            set_mtime(child, old);
        }

        let activity = bounded_recursive_last_activity_with_limits(&candidate, 8, 1).unwrap();

        assert!(
            !is_stale(activity.last_activity, now, Duration::from_secs(60 * 60)),
            "reaching the entry limit must keep the candidate rather than authorize removal"
        );
    }

    #[cfg(unix)]
    #[test]
    fn lstat_type_rejects_a_directory_symlink_for_recursion() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let link = temp.path().join("linked-target");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &link).unwrap();

        let metadata = fs::symlink_metadata(&link).unwrap();

        assert!(metadata.file_type().is_symlink());
        assert!(
            !is_real_directory(&metadata),
            "recursion eligibility must use lstat metadata, which rejects a directory symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_subtrees_do_not_contribute_activity() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("adk-candidate");
        let target = temp.path().join("external-target");
        fs::create_dir_all(&candidate).unwrap();
        fs::create_dir_all(&target).unwrap();
        let recent_file = target.join("recent.rs");
        fs::write(&recent_file, "recent activity outside candidate").unwrap();
        symlink(&target, candidate.join("linked-target")).unwrap();

        let now = SystemTime::now();
        let old = now.checked_sub(Duration::from_secs(2 * 60 * 60)).unwrap();
        set_mtime(&candidate, old);
        set_symlink_mtime(candidate.join("linked-target"), old);
        set_mtime(&recent_file, now);

        let activity = bounded_recursive_last_activity(&candidate).unwrap();

        assert!(
            is_stale(activity.last_activity, now, Duration::from_secs(60 * 60)),
            "recent activity below a symlink target must not make the candidate fresh"
        );
    }

    fn set_mtime(path: impl AsRef<Path>, modified: SystemTime) {
        set_file_mtime(path, FileTime::from_system_time(modified)).unwrap();
    }

    #[cfg(unix)]
    fn set_symlink_mtime(path: impl AsRef<Path>, modified: SystemTime) {
        let modified = FileTime::from_system_time(modified);
        filetime::set_symlink_file_times(path, modified, modified).unwrap();
    }
}
