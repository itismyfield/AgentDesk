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
/// staging/grave path and lock owner token unique so concurrent refreshes never collide.
static REFRESH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Backstop age after which a lock whose holder liveness is *indeterminate* (non-unix, or an
/// empty/unreadable/malformed owner token) is treated as abandoned. A live, readable PID is
/// never aged out. Generous because it must never race a genuinely slow-but-live refresh.
const STALE_LOCK_TTL: Duration = Duration::from_secs(300);

/// Releases a skill's refresh lock on drop (every exit path, including panic unwind) so a
/// failed refresh cannot deadlock later ones.
///
/// The release is ownership-safe: it removes the lockfile only if it still carries THIS
/// guard's exact `<pid>:<seq>` token. If a recoverer superseded us (different token) or the
/// file is already gone, it leaves the file untouched -- so even a mistaken steal can never
/// delete the new owner's lock and let a third entrant into the swap critical section.
struct SkillRefreshLock {
    path: PathBuf,
    token: String,
}

impl Drop for SkillRefreshLock {
    fn drop(&mut self) {
        match fs::read_to_string(&self.path) {
            Ok(contents) if contents.trim() == self.token => {
                let _ = fs::remove_file(&self.path);
            }
            _ => {} // gone or superseded: not ours to remove
        }
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

/// Atomically creates the lockfile, stamping a unique `<pid>:<seq>` owner token, and returns
/// `Ok(None)` if a holder already exists. The token drives both stale-owner recovery (its
/// PID) and ownership-safe release (the whole token; see [`SkillRefreshLock`]).
fn try_take_lock(lock_path: &Path) -> Result<Option<SkillRefreshLock>, String> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)
    {
        Ok(mut file) => {
            let token = format!(
                "{}:{}",
                std::process::id(),
                REFRESH_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            // A lost write leaves an empty token: liveness becomes indeterminate and the TTL
            // backstop eventually recovers it -- never a destructive early removal.
            let _ = file.write_all(token.as_bytes());
            Ok(Some(SkillRefreshLock {
                path: lock_path.to_path_buf(),
                token,
            }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(format!("Failed to lock '{}': {e}", lock_path.display())),
    }
}

/// A lock is stale (safe to steal) only when its holder is provably gone:
///   * the recorded PID is readable and confirmed NOT alive (unix `kill(pid, 0)` -> `ESRCH`),
///     or
///   * liveness is indeterminate (non-unix, or an empty/unreadable/malformed token) AND the
///     lock is older than [`STALE_LOCK_TTL`].
///
/// Liveness is authoritative: a readable, live PID is NEVER stolen regardless of age, so a
/// slow-but-active holder cannot be stolen out from under its own copy/swap.
fn skill_refresh_lock_is_stale(lock_path: &Path) -> bool {
    match read_lock_pid(lock_path).and_then(pid_liveness) {
        Some(alive) => !alive,
        None => lock_file_age(lock_path).is_some_and(|age| age >= STALE_LOCK_TTL),
    }
}

/// Parses the holder PID (the leading `<pid>` of the `<pid>:<seq>` token). `None` when the
/// token is missing/empty/malformed, i.e. liveness cannot be determined from it.
fn read_lock_pid(lock_path: &Path) -> Option<u32> {
    let contents = fs::read_to_string(lock_path).ok()?;
    contents.trim().split(':').next()?.parse::<u32>().ok()
}

fn lock_file_age(lock_path: &Path) -> Option<Duration> {
    let modified = fs::metadata(lock_path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// Probes whether `pid` is alive via `kill(pid, 0)` (delivers no signal): `Some(true)` when
/// reachable or `EPERM` (alive, not ours), `Some(false)` on `ESRCH` (gone). `None` means
/// liveness is indeterminate on this platform and the caller must fall back to the TTL.
#[cfg(unix)]
#[allow(unsafe_code)]
fn pid_liveness(pid: u32) -> Option<bool> {
    if pid == 0 {
        return Some(true); // kill(0, ...) targets our own process group; treat as alive
    }
    let reachable = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    Some(reachable || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(not(unix))]
fn pid_liveness(_pid: u32) -> Option<bool> {
    None // no cheap liveness probe here; fall back to the TTL backstop
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

#[cfg(test)]
mod tests {
    use super::*;

    /// #4256: dropping a guard whose token no longer matches the on-disk lock (it was
    /// stolen/superseded) must NOT delete that lock -- otherwise a third entrant could
    /// acquire and race the delete+rename critical section.
    #[test]
    fn superseded_guard_does_not_delete_new_owners_lock() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("demo.lock");

        // Guard stamped with token A, but the on-disk lock now carries a recoverer's token B.
        let guard = SkillRefreshLock {
            path: lock_path.clone(),
            token: "111:1".to_string(),
        };
        fs::write(&lock_path, "222:2").unwrap();
        drop(guard);
        assert_eq!(
            fs::read_to_string(&lock_path).unwrap(),
            "222:2",
            "a superseded guard must leave the new owner's lock intact"
        );

        // Sanity: a guard whose token still matches DOES release its own lock on drop.
        let guard = SkillRefreshLock {
            path: lock_path.clone(),
            token: "222:2".to_string(),
        };
        drop(guard);
        assert!(
            !lock_path.exists(),
            "a matching guard must release its own lock"
        );
    }
}
