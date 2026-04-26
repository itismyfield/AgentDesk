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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Free-byte threshold below which we mark the partition as "low".
pub const LOW_DISK_THRESHOLD_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Window in seconds after a recent ENOSPC fault during which we keep the
/// "disk full" banner up even if free-space probes recover. Gives the
/// operator a chance to see the warning even when the cause was transient
/// (e.g. a build that briefly hit the cliff and exited).
pub const ENOSPC_BANNER_LINGER_SECS: u64 = 5 * 60;

/// Process-global last ENOSPC timestamp (Unix epoch seconds, 0 = never).
/// Written by `record_enospc_now`, read by `seconds_since_last_enospc`.
static LAST_ENOSPC_EPOCH_SECS: AtomicU64 = AtomicU64::new(0);

/// Mark that a write just failed with ENOSPC. Reaches the monitoring tick
/// out-of-band so we don't have to thread a context handle through every
/// runtime_store call site.
pub fn record_enospc_now() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    LAST_ENOSPC_EPOCH_SECS.store(now, Ordering::Relaxed);
}

/// Seconds since the most recent recorded ENOSPC, or `None` if no fault has
/// ever been recorded in this process.
pub fn seconds_since_last_enospc() -> Option<u64> {
    let last = LAST_ENOSPC_EPOCH_SECS.load(Ordering::Relaxed);
    if last == 0 {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(last);
    Some(now.saturating_sub(last))
}

/// True when an ENOSPC fault was recorded within the linger window.
pub fn enospc_recent() -> bool {
    seconds_since_last_enospc().is_some_and(|elapsed| elapsed <= ENOSPC_BANNER_LINGER_SECS)
}

#[cfg(test)]
pub fn reset_enospc_for_test() {
    LAST_ENOSPC_EPOCH_SECS.store(0, Ordering::Relaxed);
}

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

/// Build a one-line operator-facing banner string from a probe + ENOSPC
/// state. Returns `None` when neither signal warrants a banner.
pub fn banner_text(snapshot: Option<DiskSpaceSnapshot>) -> Option<String> {
    let recent = enospc_recent();
    let low = snapshot.is_some_and(|s| s.is_low());
    if !recent && !low {
        return None;
    }
    let free_human = snapshot
        .map(|s| format_bytes_gib(s.free_bytes))
        .unwrap_or_else(|| "?".to_string());
    if recent {
        Some(format!(
            "💾 디스크 부족 — 최근 ENOSPC 발생, 현재 {free_human} 남음 (`/api/health` 의 `disk_*` 참조)"
        ))
    } else {
        Some(format!(
            "💾 디스크 잔여 {free_human} — 5 GiB 임계값 미만, 정리 권장"
        ))
    }
}

fn format_bytes_gib(bytes: u64) -> String {
    let gib = bytes as f64 / 1_073_741_824.0;
    if gib >= 10.0 {
        format!("{gib:.0} GiB")
    } else {
        format!("{gib:.1} GiB")
    }
}

/// Banner key under which the disk-space entry is tracked in
/// [`crate::server::routes::state::MonitoringStore`]. Stable so upsert/remove can
/// find the same row across ticks.
pub const MONITORING_BANNER_KEY: &str = "disk_space";

/// Spawn a background tick that probes the runtime partition every 30 s and
/// upserts a banner entry on every channel that already has any monitoring
/// row, recovering by removing the entry when disk health returns. Also logs
/// a tracing warning so operators on a terminal see the signal even when no
/// channel has an active banner.
///
/// The tick deliberately only touches channels that already have monitoring
/// entries. Pushing to every channel in `agentdesk.yaml` would be more
/// thorough but would create unsolicited noise on idle channels — operators
/// can read `/api/health` (`disk_*` fields) for the off-banner signal and
/// the dashboard surfaces the same info.
pub fn spawn_disk_monitor_tick(probe_path: PathBuf) {
    use std::sync::Arc;
    use tokio::time::{Duration, interval};

    tokio::spawn(async move {
        let mut iv = interval(Duration::from_secs(30));
        // Skip the first immediate tick — the probe right at boot has no
        // useful baseline yet and would race startup recovery.
        iv.tick().await;
        let store: Arc<_> = crate::server::routes::state::global_monitoring_store();
        loop {
            iv.tick().await;
            run_disk_monitor_tick_once(&probe_path, &store).await;
        }
    });
}

/// One-shot version of the monitoring tick. Exposed for tests and for
/// operators wiring a custom interval.
pub async fn run_disk_monitor_tick_once(
    probe_path: &Path,
    monitoring: &std::sync::Arc<tokio::sync::Mutex<crate::server::routes::state::MonitoringStore>>,
) {
    let snapshot = probe(probe_path);
    let banner = banner_text(snapshot);

    if let Some(message) = banner.as_deref() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] 💾 disk-monitor: {message}");
    }

    let affected_channels: Vec<u64> = {
        let store = monitoring.lock().await;
        store.tracked_channel_ids()
    };

    if affected_channels.is_empty() {
        return;
    }

    let mut store = monitoring.lock().await;
    for channel_id in affected_channels {
        if let Some(message) = banner.as_deref() {
            store.upsert(
                channel_id,
                MONITORING_BANNER_KEY.to_string(),
                message.to_string(),
            );
        } else {
            store.remove(channel_id, MONITORING_BANNER_KEY);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that touch the global `LAST_ENOSPC_EPOCH_SECS` need to
    /// serialize, otherwise concurrent runs flip the flag underneath each
    /// other.
    fn enospc_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

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

    #[test]
    fn banner_text_returns_none_when_disk_healthy_and_no_enospc() {
        let _lock = enospc_test_lock();
        reset_enospc_for_test();
        let healthy = DiskSpaceSnapshot {
            free_bytes: LOW_DISK_THRESHOLD_BYTES * 4,
            total_bytes: LOW_DISK_THRESHOLD_BYTES * 10,
        };
        assert!(banner_text(Some(healthy)).is_none());
        assert!(banner_text(None).is_none());
    }

    #[test]
    fn banner_text_warns_on_low_disk() {
        let _lock = enospc_test_lock();
        reset_enospc_for_test();
        let low = DiskSpaceSnapshot {
            free_bytes: LOW_DISK_THRESHOLD_BYTES - 1,
            total_bytes: LOW_DISK_THRESHOLD_BYTES * 4,
        };
        let banner = banner_text(Some(low)).expect("low disk must produce a banner");
        assert!(banner.contains("디스크 잔여"));
        assert!(banner.contains("GiB"));
    }

    #[test]
    fn banner_text_prefers_enospc_message_when_recent() {
        let _lock = enospc_test_lock();
        reset_enospc_for_test();
        record_enospc_now();
        let healthy = DiskSpaceSnapshot {
            free_bytes: LOW_DISK_THRESHOLD_BYTES * 4,
            total_bytes: LOW_DISK_THRESHOLD_BYTES * 10,
        };
        let banner = banner_text(Some(healthy)).expect("recent ENOSPC must override healthy probe");
        assert!(banner.contains("ENOSPC"), "banner: {banner}");
        reset_enospc_for_test();
    }

    #[test]
    fn enospc_recent_resets_after_window() {
        let _lock = enospc_test_lock();
        reset_enospc_for_test();
        let stale = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_sub(ENOSPC_BANNER_LINGER_SECS + 60);
        LAST_ENOSPC_EPOCH_SECS.store(stale, Ordering::Relaxed);
        assert!(!enospc_recent(), "stale ENOSPC must not flag as recent");
        reset_enospc_for_test();
    }
}
