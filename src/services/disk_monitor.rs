//! Free-disk-space probe for the AgentDesk runtime root.
//!
//! On ENOSPC, dcserver/claude/tmux fail to write state without any visible error, so free
//! bytes are surfaced through `/health` and a monitoring banner to warn before the cliff.
//! A `None` probe (non-Unix, syscall failure) means "unknown", not "low".

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Free-byte threshold for "low": a warning margin meant to leave room for a cargo build or
/// attachment burst between 30 s ticks, not a guarantee that one tick cannot exhaust it.
pub const LOW_DISK_THRESHOLD_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Seconds the "disk full" banner stays up after an ENOSPC fault, so a transient cause
/// (a build that briefly hit the cliff) is still visible to the operator.
pub const ENOSPC_BANNER_LINGER_SECS: u64 = 5 * 60;

/// Process-global last ENOSPC timestamp (Unix epoch seconds, 0 = never).
static LAST_ENOSPC_EPOCH_SECS: AtomicU64 = AtomicU64::new(0);

/// Mark that a write just failed with ENOSPC. Global so runtime_store call sites need no
/// context handle to reach the monitoring tick.
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
/// Returns `None` on non-Unix builds or if the syscall fails.
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
    // `statvfs`.
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

/// Banner key in [`crate::services::monitoring_store::MonitoringStore`]; stable so
/// upsert/remove hit the same row across ticks.
pub const MONITORING_BANNER_KEY: &str = "disk_space";

/// Spawn a 30 s tick that runs [`run_disk_monitor_tick_once`].
///
/// Only channels that already have monitoring rows get the banner, to avoid noise on idle
/// channels; `/api/health` (`disk_*`) carries the signal everywhere else.
pub fn spawn_disk_monitor_tick(probe_path: PathBuf) {
    use std::sync::Arc;
    use tokio::time::{Duration, interval};

    tokio::spawn(async move {
        let mut iv = interval(Duration::from_secs(30));
        // Skip the immediate first tick: a probe at boot would race startup recovery.
        iv.tick().await;
        let store: Arc<_> = crate::services::monitoring_store::global_monitoring_store();
        loop {
            iv.tick().await;
            run_disk_monitor_tick_once(&probe_path, &store).await;
        }
    });
}

/// One monitoring tick: probe, log any banner as a warning, then upsert or clear it on
/// every channel that already has a monitoring row.
pub async fn run_disk_monitor_tick_once(
    probe_path: &Path,
    monitoring: &std::sync::Arc<
        tokio::sync::Mutex<crate::services::monitoring_store::MonitoringStore>,
    >,
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
