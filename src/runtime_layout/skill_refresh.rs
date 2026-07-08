//! Concurrency-safe refresh of the managed skill cache (#4256).
//!
//! `skill_sync::ensure_managed_skill_dir` calls [`refresh_managed_skill_dir`] whenever the
//! managed copy of a skill drifts from its source. Layout preparation
//! (`ensure_runtime_layout` -> `migrate_legacy_skill_links` -> `ensure_managed_skill_dir`)
//! is reachable from concurrent server routes and CLI paths with no outer lock, so the
//! delete+copy+rename swap here must stay safe when two processes refresh the same skill.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic per-process counter that, combined with `std::process::id()`, makes every
/// staging directory path unique so concurrent refreshes never share one.
static REFRESH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Removes its lockfile on drop, releasing a skill's refresh lock on every exit path
/// (including panic unwind) so a failed refresh cannot deadlock later ones.
struct SkillRefreshLock(PathBuf);

impl Drop for SkillRefreshLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Re-copies the source skill into the managed cache through a per-invocation staging dir
/// that atomically replaces `managed_dir`, so a mid-copy failure never leaves a half-written
/// cache `discover_skill_dirs` could pick up.
///
/// Concurrency-safe (#4256): an exclusive per-skill lockfile serializes the
/// delete+copy+rename swap across processes -- if another process already holds it we skip
/// this round (that process produces the fresh copy) rather than racing. The staging path is
/// unique (pid + [`REFRESH_SEQ`]) so two refreshes can never delete, share, or expose each
/// other's staging, and it stays under `.skill-refresh` (outside the discoverable skills
/// root). The swap tolerates `managed_dir` already being gone (a concurrent winner swapped
/// first), and the staging dir is cleaned up on success and error alike.
pub(super) fn refresh_managed_skill_dir(
    root: &Path,
    skill_name: &str,
    source_skill_dir: &Path,
    managed_dir: &Path,
) -> Result<(), String> {
    let refresh_dir = root.join(".skill-refresh");
    fs::create_dir_all(&refresh_dir)
        .map_err(|e| format!("Failed to create '{}': {e}", refresh_dir.display()))?;

    let lock_path = refresh_dir.join(format!("{skill_name}.lock"));
    let lock = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(_) => SkillRefreshLock(lock_path),
        // Another process is already refreshing this skill; let it win rather than race.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(format!("Failed to lock '{}': {e}", lock_path.display())),
    };

    let staging = refresh_dir.join(format!(
        "{skill_name}.{}.{}",
        std::process::id(),
        REFRESH_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&staging); // paranoia: clear an identically-named leftover
    let result = super::skill_sync::copy_skill_dir_resolving_symlinks(source_skill_dir, &staging)
        .and_then(|()| swap_managed_skill_dir(&staging, managed_dir));
    let _ = fs::remove_dir_all(&staging); // clean up on success and error alike
    drop(lock); // release before pruning the shared dir so a peer's lockfile keeps it alive
    let _ = fs::remove_dir(&refresh_dir); // best-effort; only removes it when empty
    result
}

/// Atomically replaces `managed_dir` with `staging`. Tolerates `managed_dir` already being
/// absent (a concurrent winner removed it), so the swap never errors on a missing target.
fn swap_managed_skill_dir(staging: &Path, managed_dir: &Path) -> Result<(), String> {
    if let Err(e) = fs::remove_dir_all(managed_dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(format!(
                "Failed to remove stale managed skill dir '{}': {e}",
                managed_dir.display()
            ));
        }
    }
    if let Some(parent) = managed_dir.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    fs::rename(staging, managed_dir).map_err(|e| {
        format!(
            "Failed to move refreshed skill dir into '{}': {e}",
            managed_dir.display()
        )
    })
}
