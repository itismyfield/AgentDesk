//! Concurrency-safe refresh of the managed skill cache.
//!
//! Layout preparation reaches [`refresh_managed_skill_dir`] from concurrent server routes and
//! CLI paths with no outer lock, so its swap must be safe when two processes refresh one skill.

use super::*;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Per-process counter; with the PID it makes staging/grave paths and owner tokens unique.
static REFRESH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Backstop age for a lock whose holder liveness is indeterminate (no probe, or a bad token).
/// Generous so it never races a slow-but-live refresh.
const STALE_LOCK_TTL: Duration = Duration::from_secs(300);

/// Releases a skill's refresh lock on drop (including unwind) so a failed refresh cannot
/// deadlock later ones. Only a lockfile still carrying this guard's token is removed.
struct SkillRefreshLock {
    path: PathBuf,
    token: String,
}

impl Drop for SkillRefreshLock {
    fn drop(&mut self) {
        // Rename the lock aside and judge the file we moved: a read-then-unlink could delete
        // a recoverer's fresh lock and let a third entrant into the swap.
        let Some(dir) = self.path.parent() else {
            return;
        };
        let lock_name = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("skill.lock");
        let grave = dir.join(format!(
            "{lock_name}.release.{}.{}",
            std::process::id(),
            REFRESH_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        // NotFound => already released (or never written); nothing to do.
        if fs::rename(&self.path, &grave).is_err() {
            return;
        }
        let is_ours = fs::read_to_string(&grave)
            .map(|c| c.trim() == self.token)
            .unwrap_or(false);
        if is_ours {
            let _ = fs::remove_file(&grave);
            return;
        }
        // We moved a foreign lock (a recoverer superseded us). Restore it via hard link,
        // which fails rather than clobbering a lock re-created in the meantime.
        if fs::hard_link(&grave, &self.path).is_err() {
            tracing::warn!(
                lock = %self.path.display(),
                "skill-refresh: superseded lock re-created concurrently; discarding stale grave"
            );
        }
        let _ = fs::remove_file(&grave);
    }
}

/// Re-copies the source skill into the managed cache through a unique staging dir under
/// `.skill-refresh` that is renamed into place, so a failed copy is never discoverable.
///
/// An exclusive per-skill lockfile serializes the delete+copy+rename swap across processes.
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

    // With indeterminate liveness two refreshers can briefly overlap here. Harmless: each
    // stages a complete copy and swaps by rename; at worst `managed` is briefly absent.
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

/// Acquires the per-skill refresh lock, recovering one abandoned by a crashed holder.
/// Returns `Ok(None)` only when a live holder is refreshing this skill.
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
    // Only the rename winner recovers, so two recoverers cannot both clobber a peer's fresh
    // lock; losing the create_new afterwards means a peer beat us, so we skip.
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

/// Atomically creates the lockfile with a unique `<pid>:<seq>` owner token, or returns
/// `Ok(None)` if a holder exists. Recovery reads its PID; release matches the whole token.
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

/// Stale only when the holder is provably gone: a dead PID, or indeterminate liveness and an
/// age past [`STALE_LOCK_TTL`]. A live PID is never stolen, whatever its age.
fn skill_refresh_lock_is_stale(lock_path: &Path) -> bool {
    match read_lock_pid(lock_path).and_then(pid_liveness) {
        Some(alive) => !alive,
        None => lock_file_age(lock_path).is_some_and(|age| age >= STALE_LOCK_TTL),
    }
}

/// Parses the holder PID from a strict `<pid>:<seq>` token or a legacy bare `<pid>`. Any
/// other shape is `None`, so liveness stays indeterminate instead of trusting a garbled PID.
fn read_lock_pid(lock_path: &Path) -> Option<u32> {
    let contents = fs::read_to_string(lock_path).ok()?;
    let mut fields = contents.trim().split(':');
    let pid_field = fields.next()?;
    let pid = parse_lock_digits::<u32>(pid_field)?;
    match fields.next() {
        None => Some(pid), // legacy bare `<pid>`
        Some(seq) if parse_lock_digits::<u64>(seq).is_some() && fields.next().is_none() => {
            Some(pid)
        }
        Some(_) => None,
    }
}

/// Parses a non-empty, all-digit token field; plain `str::parse` would accept a leading `+`.
fn parse_lock_digits<T: std::str::FromStr>(field: &str) -> Option<T> {
    if field.is_empty() || !field.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    field.parse::<T>().ok()
}

fn lock_file_age(lock_path: &Path) -> Option<Duration> {
    let modified = fs::metadata(lock_path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// Probes `pid` with `kill(pid, 0)` (no signal sent): `Some(true)` when reachable or `EPERM`
/// (alive, not ours), otherwise `Some(false)` (`ESRCH`: gone).
#[cfg(unix)]
#[allow(unsafe_code)]
fn pid_liveness(pid: u32) -> Option<bool> {
    if pid == 0 {
        return Some(true); // kill(0, ...) targets our own process group; treat as alive
    }
    let reachable = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    Some(reachable || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// Windows `kill(pid, 0)`: an unsignaled handle or `ERROR_ACCESS_DENIED` means alive,
/// `ERROR_INVALID_PARAMETER` means no such PID.
#[cfg(windows)]
#[allow(unsafe_code)]
fn pid_liveness(pid: u32) -> Option<bool> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    // SAFETY: OpenProcess takes no pointers; a null return is handled before any use.
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(code) if code == ERROR_ACCESS_DENIED as i32 => Some(true),
            Some(code) if code == ERROR_INVALID_PARAMETER as i32 => Some(false),
            _ => None,
        };
    }
    // SAFETY: OpenProcess returned a new handle that nothing else owns; OwnedHandle closes it.
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    // SAFETY: the handle stays open for this zero-timeout wait.
    Some(unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_TIMEOUT)
}

#[cfg(not(any(unix, windows)))]
fn pid_liveness(_pid: u32) -> Option<bool> {
    None // no cheap liveness probe here; fall back to the TTL backstop
}

/// Replaces `managed_dir` with `staging` (remove, then rename), tolerating an already-absent
/// `managed_dir` (a concurrent winner removed it).
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

    /// A guard whose token no longer matches the on-disk lock must restore it, not delete it.
    #[test]
    fn superseded_guard_does_not_delete_new_owners_lock() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let lock_path = dir.join("demo.lock");

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
        assert!(
            release_graves(dir).is_empty(),
            "a superseded release must not leak a grave file"
        );

        let guard = SkillRefreshLock {
            path: lock_path.clone(),
            token: "222:2".to_string(),
        };
        drop(guard);
        assert!(
            !lock_path.exists(),
            "a matching guard must release its own lock"
        );
        assert!(
            release_graves(dir).is_empty(),
            "a matching release must not leak a grave file"
        );
    }

    #[test]
    fn read_lock_pid_rejects_malformed_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let p = temp.path().join("demo.lock");
        for shape in [
            "",
            "abc",
            "123:",
            "123:garbage",
            "123:456:extra",
            ":5",
            "9a:1",
            "+123",
            "+123:456",
            "123:+456",
            "-5",
            "12 3",
        ] {
            fs::write(&p, shape).unwrap();
            assert_eq!(
                read_lock_pid(&p),
                None,
                "{shape:?} must be indeterminate (None)"
            );
        }
        fs::write(&p, "123:456").unwrap();
        assert_eq!(read_lock_pid(&p), Some(123), "well-formed <pid>:<seq>");
        fs::write(&p, "789").unwrap();
        assert_eq!(read_lock_pid(&p), Some(789), "legacy bare <pid>");
        fs::write(&p, "  42:7\n").unwrap();
        assert_eq!(
            read_lock_pid(&p),
            Some(42),
            "surrounding whitespace is trimmed"
        );
    }

    fn release_graves(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".release."))
            })
            .collect()
    }
}
