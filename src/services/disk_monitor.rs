//! Lightweight free-disk-space probe for the AgentDesk runtime root.
//!
//! Issue #1203: when `/Users` (or whatever partition holds
//! `~/.adk/release/runtime/`) hits ENOSPC, dcserver/claude/tmux silently fail
//! to write inflight state, mailbox checkpoints, and tool buffers. The
//! `⏳` reaction sticks to the user message but no further progress happens
//! and operators have no early signal. Surfacing free bytes through `/health`
//! gives the dashboard and `agentdesk doctor` a way to warn before the
//! cliff.
//!
//! Implementation note: we deliberately avoid pulling in a new crate (`fs2`,
//! `nix`) because the codebase already depends on `libc` 0.2 and the Unix
//! `statvfs` syscall is sufficient. On non-Unix builds the probe returns
//! `None` and callers treat it as "unknown" rather than "low disk".
//!
//! Threshold rationale (`LOW_DISK_THRESHOLD_BYTES`): 5 GiB. Smaller than the
//! recent 47 GB cargo target/debug accident yet large enough that one round
//! of cargo build, a Discord message-attachment burst, or a tracing log
//! rotation cannot cross it inside a single 30 s tick.

use std::path::Path;

/// Free-byte threshold below which we mark the partition as "low".
pub const LOW_DISK_THRESHOLD_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Snapshot of free-space metrics for the runtime partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskSpaceSnapshot {
    /// Free bytes available to a non-root process.
    pub free_bytes: u64,
    /// Total bytes on the partition.
    pub total_bytes: u64,
}

impl DiskSpaceSnapshot {
    pub fn used_pct(self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        let used = self.total_bytes.saturating_sub(self.free_bytes) as f64;
        used / self.total_bytes as f64 * 100.0
    }

    pub fn is_low(self) -> bool {
        self.free_bytes < LOW_DISK_THRESHOLD_BYTES
    }
}

/// Probe free space for the partition that hosts `path`.
///
/// Returns `None` on non-Unix builds or if the underlying syscall fails (the
/// caller treats unknown as "no signal" rather than "low").
pub fn probe(path: &Path) -> Option<DiskSpaceSnapshot> {
    #[cfg(unix)]
    {
        unix_statvfs(path)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(unix)]
fn unix_statvfs(path: &Path) -> Option<DiskSpaceSnapshot> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let cpath = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `cpath` is a NUL-terminated path; `buf` has the right layout for
    // `statvfs`. Failure (rc != 0) is reported via `errno` which we ignore —
    // the caller treats `None` as "no signal".
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut buf) };
    if rc != 0 {
        return None;
    }
    let block_size = if buf.f_frsize > 0 {
        buf.f_frsize as u64
    } else {
        buf.f_bsize as u64
    };
    let free_bytes = (buf.f_bavail as u64).saturating_mul(block_size);
    let total_bytes = (buf.f_blocks as u64).saturating_mul(block_size);
    Some(DiskSpaceSnapshot {
        free_bytes,
        total_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_pct_handles_empty_partition() {
        let snapshot = DiskSpaceSnapshot {
            free_bytes: 0,
            total_bytes: 0,
        };
        assert_eq!(snapshot.used_pct(), 0.0);
        assert!(snapshot.is_low());
    }

    #[test]
    fn used_pct_reports_partition_fill() {
        let snapshot = DiskSpaceSnapshot {
            free_bytes: 1_000,
            total_bytes: 10_000,
        };
        // 9_000 / 10_000 = 90 %.
        assert!((snapshot.used_pct() - 90.0).abs() < 1e-9);
    }

    #[test]
    fn is_low_pinned_to_threshold() {
        let just_above = DiskSpaceSnapshot {
            free_bytes: LOW_DISK_THRESHOLD_BYTES,
            total_bytes: LOW_DISK_THRESHOLD_BYTES * 2,
        };
        let just_below = DiskSpaceSnapshot {
            free_bytes: LOW_DISK_THRESHOLD_BYTES - 1,
            total_bytes: LOW_DISK_THRESHOLD_BYTES * 2,
        };
        assert!(!just_above.is_low());
        assert!(just_below.is_low());
    }

    #[test]
    fn probe_returns_value_for_existing_root() {
        // The real probe must succeed on a Unix CI host; on non-Unix we expect
        // `None` (skip the assertion). Either way the call must not panic.
        let snapshot = probe(std::path::Path::new("/"));
        if cfg!(unix) {
            let snapshot = snapshot.expect("statvfs(/) should succeed on a Unix host");
            assert!(snapshot.total_bytes > 0);
        } else {
            assert!(snapshot.is_none());
        }
    }
}
