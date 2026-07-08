//! Concurrency-safe refresh of the managed skill cache (#4256).
//!
//! `skill_sync::ensure_managed_skill_dir` calls [`refresh_managed_skill_dir`] whenever the
//! managed copy of a skill drifts from its source. Layout preparation
//! (`ensure_runtime_layout` -> `migrate_legacy_skill_links` -> `ensure_managed_skill_dir`)
//! is reachable from concurrent server routes and CLI paths with no outer lock, so the
//! delete+copy+rename swap here must stay safe when two processes refresh the same skill.

use super::*;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Monotonic per-process counter that, combined with `std::process::id()`, makes every
/// staging (and grave) directory path unique so concurrent refreshes never share one.
static REFRESH_SEQ: AtomicU64 = AtomicU64::new(0);

/// A lock older than this is treated as abandoned. A refresh takes milliseconds, so this is
/// a generous backstop that recovers a crash-orphaned lock even when its PID has been reused.
const STALE_LOCK_TTL: Duration = Duration::from_secs(60);

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

    // A live holder means another process is refreshing this skill; skip and let it win.
    let Some(lock) = acquire_skill_refresh_lock(&refresh_dir, skill_name)? else {
        return Ok(());
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

/// Acquires the per-skill refresh lock, recovering a lock abandoned by a crashed holder so a
/// dead process can never wedge refresh forever (#4256). Returns `Ok(None)` only when a
/// genuinely live holder is refreshing this skill (skip and let it produce the fresh copy).
fn acquire_skill_refresh_lock(
    refresh_dir: &Path,
    skill_name: &str,
) -> Result<Option<SkillRefreshLock>, String> {
    let lock_path = refresh_dir.join(format!("{skill_name}.lock"));
    if let Some(lock) = try_take_lock(&lock_path)? {
        return Ok(Some(lock));
    }
    if !skill_refresh_lock_is_stale(&lock_path) {
        return Ok(None);
    }
    // Atomically claim removal of the stale lock: whoever wins the rename is the unique
    // recoverer, so two simultaneous recoverers can never both clobber a peer's fresh lock
    // (only the rename winner touches it). Then take the lock; losing the still-exclusive
    // create_new means a peer beat us to it, so we skip.
    let grave = refresh_dir.join(format!(
        "{skill_name}.lock.dead.{}.{}",
        std::process::id(),
        REFRESH_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if fs::rename(&lock_path, &grave).is_ok() {
        let _ = fs::remove_file(&grave);
    }
    try_take_lock(&lock_path)
}

/// Atomically creates the lockfile, returning `Ok(None)` if a holder already exists.
fn try_take_lock(lock_path: &Path) -> Result<Option<SkillRefreshLock>, String> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)
    {
        Ok(mut file) => {
            // Best-effort PID stamp for stale-owner recovery; the TTL backstop covers a lost
            // write.
            let _ = file.write_all(std::process::id().to_string().as_bytes());
            Ok(Some(SkillRefreshLock(lock_path.to_path_buf())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(format!("Failed to lock '{}': {e}", lock_path.display())),
    }
}

/// A lock is stale (safe to steal) when its recorded holder PID is not alive, or -- as a
/// backstop for PID reuse and pre-stamp locks -- when it is older than [`STALE_LOCK_TTL`].
fn skill_refresh_lock_is_stale(lock_path: &Path) -> bool {
    if let Ok(contents) = fs::read_to_string(lock_path) {
        if let Ok(pid) = contents.trim().parse::<u32>() {
            if !holder_pid_is_alive(pid) {
                return true;
            }
        }
    }
    lock_file_age(lock_path).is_some_and(|age| age >= STALE_LOCK_TTL)
}

fn lock_file_age(lock_path: &Path) -> Option<Duration> {
    let modified = fs::metadata(lock_path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// `kill(pid, 0)` probes existence without delivering a signal: success or `EPERM` means the
/// process is alive, `ESRCH` means it is gone.
#[cfg(unix)]
#[allow(unsafe_code)]
fn holder_pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return true; // kill(0, ...) targets our own process group; never treat as stale
    }
    let reachable = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    reachable || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn holder_pid_is_alive(_pid: u32) -> bool {
    true // no cheap liveness probe here; the TTL backstop still recovers abandoned locks
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
