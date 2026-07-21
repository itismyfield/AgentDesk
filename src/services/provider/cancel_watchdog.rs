use super::cancel_token_cleanup::executor::{CleanupRequest, TmuxCleanupIntent};
use super::{CancelSource, CancelToken};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

pub(super) fn current_unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

pub(super) fn enforce_watchdog_deadline(token: &CancelToken, now_ms: i64) -> bool {
    let deadline_ms = token.watchdog_deadline_ms.load(Ordering::Relaxed);
    if deadline_ms > 0 && now_ms >= deadline_ms && !token.is_async_managed() {
        return token.try_mark_watchdog_timeout();
    }
    false
}

/// Poll one cancellation boundary. The token remains the sole owner of its target.
pub(crate) fn poll_cancel_watchdog(token: &CancelToken, label: &'static str, now_ms: i64) -> bool {
    if token.is_completion_cleanup() {
        tracing::debug!(
            provider_cancel_watchdog = label,
            cancel_source = ?token.cancel_source(),
            cancel_source_kind = ?token.cancel_source_kind(),
            "cancel watchdog exiting after normal completion cleanup"
        );
        return true;
    }
    let deadline_enforced = enforce_watchdog_deadline(token, now_ms);
    if token.is_completion_cleanup() {
        return true;
    }
    if !token.cancelled.load(Ordering::Acquire) {
        return false;
    }

    let cleanup_outcome = token.request_cleanup(CleanupRequest {
        cancel_source: if deadline_enforced {
            "watchdog_timeout"
        } else {
            "provider_cancel_dispatch"
        }
        .to_string(),
        intent: TmuxCleanupIntent::PidOnly,
        termination_reason: None,
        hard_stop_target: None,
    });
    tracing::warn!(
        provider_cancel_watchdog = label,
        cancel_source = ?token.cancel_source(),
        cancel_source_kind = ?token.cancel_source_kind(),
        ?cleanup_outcome,
        "cancel watchdog dispatched token-owned cleanup"
    );
    !cleanup_outcome.retry_pid_cleanup
}

pub struct CancelWatchdog {
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CancelWatchdog {
    fn new(done: Arc<AtomicBool>, handle: JoinHandle<()>) -> Self {
        Self {
            done,
            handle: Some(handle),
        }
    }
}

impl Drop for CancelWatchdog {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn spawn_cancel_watchdog(
    token: Option<Arc<CancelToken>>,
    label: &'static str,
) -> Option<CancelWatchdog> {
    let token = token?;
    let done = Arc::new(AtomicBool::new(false));
    let done_for_thread = Arc::clone(&done);
    let handle = std::thread::spawn(move || {
        while !done_for_thread.load(Ordering::Relaxed) {
            if poll_cancel_watchdog(&token, label, current_unix_millis()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    Some(CancelWatchdog::new(done, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider::cancel_token_cleanup::executor::{
        pid_kill_dispatches_for_test, with_executor_dispatch_seam,
    };

    #[test]
    fn deadline_poll_dispatches_token_current_pid_without_raw_pid_argument() {
        with_executor_dispatch_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid(std::process::id());
            token.watchdog_deadline_ms.store(100, Ordering::Relaxed);

            assert!(poll_cancel_watchdog(&token, "test-watchdog", 100));
            assert_eq!(pid_kill_dispatches_for_test(), 1);
            assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 1);
            assert_eq!(token.cancel_source().as_deref(), Some("watchdog_timeout"));
            assert_eq!(
                token.cancel_source_kind(),
                Some(CancelSource::WatchdogTimeout)
            );
        });
    }

    #[test]
    fn completion_cleanup_after_deadline_skips_timeout_attribution_and_dispatch() {
        with_executor_dispatch_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid(4712);
            token.watchdog_deadline_ms.store(100, Ordering::Relaxed);
            token.mark_completion_cleanup();

            assert!(poll_cancel_watchdog(&token, "test-watchdog", 100));
            assert_eq!(pid_kill_dispatches_for_test(), 0);
            assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 0);
            assert_eq!(token.cancel_source_kind(), None);
            assert!(!token.cancelled.load(Ordering::Acquire));
        });
    }

    #[test]
    fn completion_publication_wins_interleaving_before_timeout_commit() {
        let token = Arc::new(CancelToken::new());
        token.watchdog_deadline_ms.store(100, Ordering::Relaxed);
        let publication = token
            .cancellation_publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let timeout_token = Arc::clone(&token);
        let timeout = std::thread::spawn(move || timeout_token.try_mark_watchdog_timeout());
        token.completion_cleanup.store(true, Ordering::Release);
        drop(publication);

        assert!(!timeout.join().expect("timeout thread should finish"));
        assert_eq!(token.cancel_source_kind(), None);
        assert!(!token.cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn async_managed_deadline_remains_unhandled() {
        let token = CancelToken::new();
        token.mark_async_managed();
        token.watchdog_deadline_ms.store(100, Ordering::Relaxed);

        assert!(!poll_cancel_watchdog(&token, "test-watchdog", 100));
        assert!(!token.cancelled.load(Ordering::Relaxed));
        assert_eq!(token.cancel_source_kind(), None);
    }

    #[test]
    fn external_cancel_before_deadline_does_not_become_watchdog_timeout() {
        with_executor_dispatch_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid(std::process::id());
            token.watchdog_deadline_ms.store(200, Ordering::Relaxed);
            token.cancelled.store(true, Ordering::Relaxed);

            assert!(poll_cancel_watchdog(&token, "test-watchdog", 100));
            assert_eq!(pid_kill_dispatches_for_test(), 1);
            assert_eq!(
                token.cancel_source().as_deref(),
                Some("provider_cancel_dispatch")
            );
            assert_eq!(token.cancel_source_kind(), Some(CancelSource::Other));
        });
    }

    #[test]
    fn delayed_watchdog_preserves_existing_external_cancel_source() {
        with_executor_dispatch_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid(std::process::id());
            token.watchdog_deadline_ms.store(100, Ordering::Relaxed);
            token.publish_cancel("voice_barge_in_explicit_stop");

            assert!(poll_cancel_watchdog(&token, "test-watchdog", 200));
            assert_eq!(pid_kill_dispatches_for_test(), 1);
            assert_eq!(
                token.cancel_source().as_deref(),
                Some("voice_barge_in_explicit_stop")
            );
            assert_eq!(token.cancel_source_kind(), Some(CancelSource::UserBargeIn));
        });
    }

    #[test]
    fn external_cancel_publication_wins_interleaving_before_timeout_commit() {
        let token = Arc::new(CancelToken::new());
        token.watchdog_deadline_ms.store(100, Ordering::Relaxed);
        let publication = token
            .cancellation_publication
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let timeout_token = Arc::clone(&token);
        let timeout = std::thread::spawn(move || timeout_token.try_mark_watchdog_timeout());
        token.set_cancel_source_locked("voice_barge_in_explicit_stop");
        token.cancelled.store(true, Ordering::Release);
        drop(publication);

        assert!(!timeout.join().expect("timeout thread should finish"));
        assert_eq!(token.cancel_source_kind(), Some(CancelSource::UserBargeIn));
        assert_eq!(
            token.cancel_source().as_deref(),
            Some("voice_barge_in_explicit_stop")
        );
    }

    #[test]
    fn failed_pid_cleanup_keeps_watchdog_alive_for_retry() {
        use crate::services::provider::cancel_token_cleanup::executor::set_pid_kill_succeeds_for_test;

        with_executor_dispatch_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid(std::process::id());
            token.cancelled.store(true, Ordering::Relaxed);
            set_pid_kill_succeeds_for_test(false);

            assert!(!poll_cancel_watchdog(&token, "test-watchdog", 0));
            assert_eq!(pid_kill_dispatches_for_test(), 1);
            assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 0);

            set_pid_kill_succeeds_for_test(true);
            assert!(poll_cancel_watchdog(&token, "test-watchdog", 1));
            assert_eq!(pid_kill_dispatches_for_test(), 2);
            assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 1);
        });
    }

    #[test]
    fn cleanup_does_not_replace_existing_specific_cancel_source() {
        with_executor_dispatch_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid(std::process::id());
            token.set_cancel_source("voice_barge_in_explicit_stop");
            token.cancelled.store(true, Ordering::Relaxed);

            assert!(poll_cancel_watchdog(&token, "test-watchdog", 0));
            assert_eq!(
                token.cancel_source().as_deref(),
                Some("voice_barge_in_explicit_stop")
            );
            assert_eq!(token.cancel_source_kind(), Some(CancelSource::UserBargeIn));
        });
    }
}
