use super::CancelToken;
use super::cancel_token_cleanup::executor::{CleanupRequest, TmuxCleanupIntent};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

/// Poll one cancellation boundary. The token remains the sole owner of its target.
pub(crate) fn poll_cancel_watchdog(token: &CancelToken, label: &'static str) -> bool {
    if token.is_completion_cleanup() {
        tracing::debug!(
            provider_cancel_watchdog = label,
            cancel_source = ?token.cancel_source(),
            cancel_source_kind = ?token.cancel_source_kind(),
            "cancel watchdog exiting after normal completion cleanup"
        );
        return true;
    }
    if !token.cancelled.load(Ordering::Acquire) {
        return false;
    }

    let cleanup_outcome = token.request_cleanup(CleanupRequest {
        cancel_source: "provider_cancel_dispatch".to_string(),
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
            if poll_cancel_watchdog(&token, label) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    Some(CancelWatchdog::new(done, handle))
}

#[cfg(test)]
mod tests {
    #[test]
    fn live_token_never_dispatches_cleanup_without_cancel() {
        with_executor_dispatch_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid(std::process::id());
            for _ in 0..24 {
                assert!(!poll_cancel_watchdog(&token, "long-active-turn"));
            }
            assert_eq!(pid_kill_dispatches_for_test(), 0);
            assert!(!crate::services::provider::cancel_requested(Some(&token)));
            assert_eq!(token.cancel_source(), None);
        });
    }

    use super::*;
    use crate::services::provider::CancelSource;
    use crate::services::provider::cancel_token_cleanup::executor::{
        pid_kill_dispatches_for_test, with_executor_dispatch_seam,
    };

    #[test]
    fn explicit_cancel_dispatches_token_current_pid_without_raw_pid_argument() {
        with_executor_dispatch_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid(std::process::id());

            token.publish_cancel("manual_cancel");
            assert!(poll_cancel_watchdog(&token, "test-watchdog"));
            assert_eq!(pid_kill_dispatches_for_test(), 1);
            assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 1);
            assert_eq!(token.cancel_source().as_deref(), Some("manual_cancel"));
            assert_eq!(token.cancel_source_kind(), Some(CancelSource::Other));
        });
    }

    #[test]
    fn completion_cleanup_skips_cancel_attribution_and_dispatch() {
        with_executor_dispatch_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid(4712);
            token.mark_completion_cleanup();

            assert!(poll_cancel_watchdog(&token, "test-watchdog"));
            assert_eq!(pid_kill_dispatches_for_test(), 0);
            assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 0);
            assert_eq!(token.cancel_source_kind(), None);
            assert!(!token.cancelled.load(Ordering::Acquire));
        });
    }

    #[test]
    fn external_cancel_without_source_uses_dispatch_label() {
        with_executor_dispatch_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid(std::process::id());
            token.cancelled.store(true, Ordering::Relaxed);

            assert!(poll_cancel_watchdog(&token, "test-watchdog"));
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
            token.publish_cancel("voice_barge_in_explicit_stop");

            assert!(poll_cancel_watchdog(&token, "test-watchdog"));
            assert_eq!(pid_kill_dispatches_for_test(), 1);
            assert_eq!(
                token.cancel_source().as_deref(),
                Some("voice_barge_in_explicit_stop")
            );
            assert_eq!(token.cancel_source_kind(), Some(CancelSource::UserBargeIn));
        });
    }

    #[test]
    fn failed_pid_cleanup_keeps_watchdog_alive_for_retry() {
        use crate::services::provider::cancel_token_cleanup::executor::set_pid_kill_succeeds_for_test;

        with_executor_dispatch_seam(|| {
            let token = CancelToken::new();
            token.store_child_pid(std::process::id());
            token.cancelled.store(true, Ordering::Relaxed);
            set_pid_kill_succeeds_for_test(false);

            assert!(!poll_cancel_watchdog(&token, "test-watchdog"));
            assert_eq!(pid_kill_dispatches_for_test(), 1);
            assert_eq!(token.pid_kill_claim.load(Ordering::Acquire), 0);

            set_pid_kill_succeeds_for_test(true);
            assert!(poll_cancel_watchdog(&token, "test-watchdog"));
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

            assert!(poll_cancel_watchdog(&token, "test-watchdog"));
            assert_eq!(
                token.cancel_source().as_deref(),
                Some("voice_barge_in_explicit_stop")
            );
            assert_eq!(token.cancel_source_kind(), Some(CancelSource::UserBargeIn));
        });
    }
}
