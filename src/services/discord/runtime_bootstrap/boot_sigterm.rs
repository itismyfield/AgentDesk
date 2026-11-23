//! Process-wide SIGTERM mark, set by the runtime SIGTERM handlers. A runtime that has no handler
//! yet reads it; nothing here registers a signal, so a lone runtime keeps the default action.
use super::*;
use std::sync::LazyLock;
use tokio::sync::watch;

static MARK: LazyLock<watch::Sender<bool>> = LazyLock::new(|| watch::channel(false).0);

/// A SIGTERM handler saw the signal.
#[cfg(unix)]
pub(super) fn mark() {
    MARK.send_replace(true);
}

pub(super) fn seen() -> bool {
    *MARK.borrow()
}

#[cfg(unix)]
pub(super) fn watch() -> watch::Receiver<bool> {
    MARK.subscribe()
}

/// Resolves once the mark is set; a closed channel never resolves.
#[cfg(unix)]
pub(super) async fn signalled(mark: &mut watch::Receiver<bool>) {
    if mark.wait_for(|seen| *seen).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// A runtime leaving run_bot without a handler gives up its barrier slot once. It ends the
/// process only for the last slot after SIGTERM; otherwise a handler or the default action does.
pub(super) fn release_boot_slot(shared: &SharedData, sigterm_seen: bool) {
    if shared
        .restart
        .shutdown_counted
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
        && shared
            .restart
            .shutdown_remaining
            .fetch_sub(1, Ordering::AcqRel)
            == 1
        && sigterm_seen
    {
        std::process::exit(0);
    }
}

/// The boot gate stops on a SIGTERM a peer handler saw; no live-runtime state exists to save.
#[cfg(unix)]
pub(super) fn stop_before_start(shared: &SharedData, sigterm_seen: bool) {
    shared.restart.legacy_sigterm();
    release_boot_slot(shared, sigterm_seen);
}
