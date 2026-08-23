#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeadlessDeliveryOutcome {
    Delivered,
    Cancelled,
    Ambiguous { surfaced_error: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadlessDeliveryDisposition<'a> {
    Commit,
    PreserveForRetry { surfaced_error: Option<&'a str> },
}

pub(crate) fn headless_delivery_disposition(
    outcome: &HeadlessDeliveryOutcome,
) -> HeadlessDeliveryDisposition<'_> {
    match outcome {
        HeadlessDeliveryOutcome::Delivered | HeadlessDeliveryOutcome::Cancelled => {
            HeadlessDeliveryDisposition::Commit
        }
        HeadlessDeliveryOutcome::Ambiguous { surfaced_error } => {
            HeadlessDeliveryDisposition::PreserveForRetry {
                surfaced_error: surfaced_error.as_deref(),
            }
        }
    }
}

pub(crate) fn preserve_ambiguous_headless_delivery_for_retry(
    preserve_inflight_for_cleanup_retry: &mut bool,
) {
    *preserve_inflight_for_cleanup_retry = true;
}

pub(super) fn classify_visible_outbox_result(
    result: Result<(), String>,
) -> HeadlessDeliveryOutcome {
    match result {
        Ok(()) => HeadlessDeliveryOutcome::Delivered,
        Err(error) => HeadlessDeliveryOutcome::Ambiguous {
            surfaced_error: Some(error),
        },
    }
}

pub(super) async fn run_headless_direct_fallback<F, Fut>(
    suppress_for_cancel: bool,
    direct_fallback: F,
) -> HeadlessDeliveryOutcome
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    if suppress_for_cancel {
        return HeadlessDeliveryOutcome::Cancelled;
    }
    match direct_fallback().await {
        Ok(()) => HeadlessDeliveryOutcome::Delivered,
        Err(error) => HeadlessDeliveryOutcome::Ambiguous {
            surfaced_error: Some(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_headless_outcomes_preserve_commit_dispositions() {
        for outcome in [
            HeadlessDeliveryOutcome::Delivered,
            HeadlessDeliveryOutcome::Cancelled,
        ] {
            assert_eq!(
                headless_delivery_disposition(&outcome),
                HeadlessDeliveryDisposition::Commit
            );
        }

        for outcome in [
            HeadlessDeliveryOutcome::Ambiguous {
                surfaced_error: None,
            },
            HeadlessDeliveryOutcome::Ambiguous {
                surfaced_error: Some("visible delivery unknown".to_string()),
            },
        ] {
            assert!(matches!(
                headless_delivery_disposition(&outcome),
                HeadlessDeliveryDisposition::PreserveForRetry { .. }
            ));
            let mut preserve = false;
            preserve_ambiguous_headless_delivery_for_retry(&mut preserve);
            assert!(preserve);
        }
    }

    #[test]
    fn no_row_and_visible_wait_failure_remain_ambiguous() {
        let no_row = HeadlessDeliveryOutcome::Ambiguous {
            surfaced_error: None,
        };
        let wait_failure = classify_visible_outbox_result(Err("visibility timeout".to_string()));
        assert_eq!(
            wait_failure,
            HeadlessDeliveryOutcome::Ambiguous {
                surfaced_error: Some("visibility timeout".to_string())
            }
        );
        for outcome in [&no_row, &wait_failure] {
            assert!(matches!(
                headless_delivery_disposition(outcome),
                HeadlessDeliveryDisposition::PreserveForRetry { .. }
            ));
            let mut preserve = false;
            preserve_ambiguous_headless_delivery_for_retry(&mut preserve);
            assert!(preserve);
        }
        assert_eq!(
            classify_visible_outbox_result(Ok(())),
            HeadlessDeliveryOutcome::Delivered
        );
    }

    #[tokio::test]
    async fn pg_error_direct_fallback_success_is_delivered() {
        let outcome = run_headless_direct_fallback(false, || async { Ok(()) }).await;
        assert_eq!(outcome, HeadlessDeliveryOutcome::Delivered);
    }

    #[tokio::test]
    async fn direct_fallback_helper_keeps_cancel_and_success_contract() {
        let delivered = run_headless_direct_fallback(false, || async { Ok(()) }).await;
        assert_eq!(delivered, HeadlessDeliveryOutcome::Delivered);

        let fallback_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = fallback_called.clone();
        let cancelled = run_headless_direct_fallback(true, move || async move {
            observed.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert_eq!(cancelled, HeadlessDeliveryOutcome::Cancelled);
        assert!(!fallback_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn pg_error_cancel_suppresses_direct_fallback_and_is_cancelled() {
        let fallback_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = fallback_called.clone();
        let outcome = run_headless_direct_fallback(true, move || async move {
            observed.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .await;

        assert_eq!(outcome, HeadlessDeliveryOutcome::Cancelled);
        assert!(!fallback_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn pg_error_direct_fallback_failure_is_ambiguous_and_surfaces_error() {
        let outcome = run_headless_direct_fallback(false, || async {
            Err("direct fallback failed".to_string())
        })
        .await;

        assert_eq!(
            outcome,
            HeadlessDeliveryOutcome::Ambiguous {
                surfaced_error: Some("direct fallback failed".to_string())
            }
        );
    }
}
