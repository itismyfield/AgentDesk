//! Process-wide SIGTERM latch: armed by the first runtime that waits for shutdown, it keeps the
//! signal for runtimes that wait later, such as a native boot gate and the handler after it.
use super::*;
use std::sync::OnceLock;
use tokio::sync::watch;

static LATCH: OnceLock<watch::Sender<bool>> = OnceLock::new();

/// Arms the latch once per process; the receiver reports every SIGTERM since then.
pub(super) fn subscribe() -> watch::Receiver<bool> {
    LATCH
        .get_or_init(|| {
            let (latch, _) = watch::channel(false);
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    let latch = latch.clone();
                    tokio::spawn(async move {
                        if sigterm.recv().await.is_some() {
                            latch.send_replace(true);
                        }
                    });
                }
                Err(error) => tracing::warn!(%error, "SIGTERM listener unavailable"),
            }
            latch
        })
        .subscribe()
}

/// Resolves once the latch is set; a closed latch never resolves.
pub(super) async fn signalled(latch: &mut watch::Receiver<bool>) {
    if latch.wait_for(|seen| *seen).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// A runtime that stops before its own handler exists gives up its barrier slot once,
/// and the holder of the last slot ends the process like the SIGTERM handler.
pub(super) fn stop_before_start(shared: &SharedData) {
    shared.restart.legacy_sigterm();
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
    {
        std::process::exit(0);
    }
}

/// A runtime that leaves run_bot without a handler still answers the shutdown barrier.
pub(super) fn release_slot_on_sigterm(shared: Arc<SharedData>, mut latch: watch::Receiver<bool>) {
    tokio::spawn(async move {
        signalled(&mut latch).await;
        stop_before_start(&shared);
    });
}
