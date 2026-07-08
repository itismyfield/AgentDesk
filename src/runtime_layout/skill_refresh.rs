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
/// guard's exact `<pid>:<seq>` token, so even a mistaken steal can never delete the new
/// owner's lock and let a third entrant into the swap critical section.
struct SkillRefreshLock {
    path: PathBuf,
    token: String,
}

impl Drop for SkillRefreshLock {
    fn drop(&mut self) {
        // Atomic compare-and-remove, not a read-then-unlink TOCTOU: we first `rename` the
        // lock aside to a unique grave, so we then inspect and act on THE EXACT FILE WE
        // MOVED -- never "whatever happens to be at lock_path now" (which a recoverer could
        // have replaced between a naive read and unlink, so the unlink would delete the
        // recoverer's fresh lock and reopen the third-entrant hole).
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
        // We moved a FOREIGN lock (a recoverer superseded us on the indeterminate path).
        // Restore it create-exclusively via a hard link so a third party's freshly
        // re-created lock is never clobbered; then drop our extra grave name. If the slot is
        // already retaken (hard_link EEXIST), the new owner keeps its lock and we warn
        // rather than silently discarding the graved token.
        if fs::hard_link(&grave, &self.path).is_err() {
            tracing::warn!(
                lock = %self.path.display(),
                "skill-refresh: superseded lock re-created concurrently; discarding stale grave"
            );
        }
        let _ = fs::remove_file(&grave);
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

    // Residual-risk bound: the rename-based release (see SkillRefreshLock::drop) has a
    // sub-instant where lock_path is absent, so on the INDETERMINATE path only (non-unix, or
    // a malformed/empty token -- NEVER on unix with a well-formed token, where liveness is
    // authoritative and a live holder is never stolen) two refreshers can transiently
    // overlap in the copy/swap below. This is NOT corrupting: each refresher's staging dir is
    // unique and a COMPLETE copy of the same source, the swap is an atomic full-dir rename,
    // so both converge on identical correct content. The worst case is a transient absent
    // `managed` dir plus a redundant copy, which self-heals on the next ensure_managed_skill_dir.
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

/// Parses the holder PID from a STRICT `<pid>:<seq>` owner token -- both fields non-empty
/// and pure ASCII digits, exactly two colon-separated fields -- or the legacy bare `<pid>`
/// stamp (pure ASCII digits, no colon). Every other shape (empty, sign-prefixed like
/// `+123`/`-5`, `123:`, `123:garbage`, `123:456:extra`, embedded spaces, trailing junk)
/// returns `None` so liveness stays indeterminate and the caller falls back to the TTL
/// branch rather than trusting a garbled PID. The digit check is required because
/// `str::parse::<u32>` would otherwise accept a leading `+`.
fn read_lock_pid(lock_path: &Path) -> Option<u32> {
    let contents = fs::read_to_string(lock_path).ok()?;
    let mut fields = contents.trim().split(':');
    let pid_field = fields.next()?;
    let pid = parse_lock_digits::<u32>(pid_field)?;
    match fields.next() {
        None => Some(pid), // legacy bare `<pid>`
        // `<pid>:<seq>`: seq must be pure digits and the final field (reject extra fields).
        Some(seq) if parse_lock_digits::<u64>(seq).is_some() && fields.next().is_none() => {
            Some(pid)
        }
        Some(_) => None,
    }
}

/// Parses a lock-token field only when it is non-empty and composed solely of ASCII digits,
/// so no sign prefix (`+`/`-`), space, or other junk `str::parse` might tolerate slips through.
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
    /// acquire and race the delete+rename critical section. The atomic rename-based release
    /// moves the file aside, sees a foreign token, and restores it intact (no leftover grave).
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
        assert!(
            release_graves(dir).is_empty(),
            "a matching release must not leak a grave file"
        );
    }

    /// #4256 Finding A: only a strict `<pid>:<seq>` token or a legacy bare `<pid>` yields a
    /// PID; every malformed shape stays indeterminate (`None`) so the caller uses the TTL.
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
