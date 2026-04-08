use super::*;

/// Minimum interval between Discord placeholder edits for progress status.
/// Configurable via AGENTDESK_STATUS_INTERVAL_SECS env var. Default: 5 seconds.
pub(super) fn status_update_interval() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let secs = std::env::var("AGENTDESK_STATUS_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(5);
        Duration::from_secs(secs)
    })
}

/// Turn watchdog timeout. Configurable via AGENTDESK_TURN_TIMEOUT_SECS env var.
/// Default: 3600 seconds (60 minutes).
pub(super) fn turn_watchdog_timeout() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let secs = std::env::var("AGENTDESK_TURN_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(3600);
        Duration::from_secs(secs)
    })
}

/// Global watchdog deadline overrides, keyed by channel_id.
/// Written by POST /api/turns/{channel_id}/extend-timeout, read by the watchdog loop.
/// Values are Unix timestamp in milliseconds representing the new deadline.
static WATCHDOG_DEADLINE_OVERRIDES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<u64, i64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Extend the watchdog deadline for a channel. Returns the new deadline_ms or None if at cap.
pub fn extend_watchdog_deadline(channel_id: u64, extend_by_secs: u64) -> Option<i64> {
    let extend_ms = extend_by_secs as i64 * 1000;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut map = WATCHDOG_DEADLINE_OVERRIDES.lock().ok()?;
    let current = map.get(&channel_id).copied().unwrap_or(now_ms);
    let new_deadline = std::cmp::max(current, now_ms) + extend_ms;
    // Don't enforce max here — the watchdog will clamp against its own max
    map.insert(channel_id, new_deadline);
    Some(new_deadline)
}

/// Read and consume the deadline override for a channel (if any).
pub(super) fn take_watchdog_deadline_override(channel_id: u64) -> Option<i64> {
    WATCHDOG_DEADLINE_OVERRIDES.lock().ok()?.remove(&channel_id)
}

/// Remove the deadline override for a channel (on turn completion).
pub(super) fn clear_watchdog_deadline_override(channel_id: u64) {
    if let Ok(mut map) = WATCHDOG_DEADLINE_OVERRIDES.lock() {
        map.remove(&channel_id);
    }
}
/// Check if a deferred restart has been requested and no active or finalizing turns remain
/// **across all providers**.
///
/// `global_active` / `global_finalizing` are process-wide counters shared by every provider.
/// A single provider draining to zero is NOT sufficient — we must wait for every provider.
/// `shutdown_remaining` ensures all providers finish saving before any calls `exit(0)`.
/// `shutdown_counted` (per-provider) prevents double-decrement when both deferred restart
/// and SIGTERM paths run for the same provider.
pub(super) fn check_deferred_restart(shared: &SharedData) {
    let g_active = shared
        .global_active
        .load(std::sync::atomic::Ordering::Relaxed);
    let g_finalizing = shared
        .global_finalizing
        .load(std::sync::atomic::Ordering::Relaxed);
    if g_active > 0 || g_finalizing > 0 {
        return;
    }
    if !shared
        .restart_pending
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    // CAS: ensure this provider only decrements once
    if shared
        .shutdown_counted
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Relaxed,
        )
        .is_err()
    {
        return;
    }
    // Only the last provider to finish calls exit(0)
    if shared
        .shutdown_remaining
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
        == 1
    {
        let Some(root) = crate::agentdesk_runtime_root() else {
            return;
        };
        let marker = root.join("restart_pending");
        let version = fs::read_to_string(&marker).unwrap_or_default();
        let version = version.trim();
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] 🔄 Deferred restart: all turns complete, restarting for v{version}...");
        let _ = fs::remove_file(&marker);
        std::process::exit(0);
    }
}
