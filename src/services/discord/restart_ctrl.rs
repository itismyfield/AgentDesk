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
/// Legacy restart helper retained for source compatibility. #2713 changed
/// restart semantics to quick-exit + rehydrate, so callers must persist
/// queue/checkpoint state before invoking this and must not wait for turns
/// to drain here.
pub(super) fn check_deferred_restart(shared: &SharedData) {
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
    if shared
        .shutdown_remaining
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
        != 1
    {
        return;
    }
    let version = crate::agentdesk_runtime_root()
        .map(|root| root.join("restart_pending"))
        .and_then(|marker| {
            let version = fs::read_to_string(&marker).unwrap_or_default();
            let _ = fs::remove_file(&marker);
            Some(version)
        })
        .unwrap_or_default();
    let version = version.trim();
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] 🔄 Deferred restart quick-exit requested for v{version}");
    std::process::exit(0);
}
