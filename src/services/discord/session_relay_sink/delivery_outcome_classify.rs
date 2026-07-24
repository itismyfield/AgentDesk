//! #4046 S1r-1 P2: pure classification of a session-bound short-replace controller
//! outcome that did NOT confirm a placeholder edit. Extracted out of the giant,
//! #3016-hot `session_relay_sink.rs` (frozen prod-LoC baseline) so this
//! retry-classification logic stays unit-testable without re-inflating that file.

use crate::services::cluster::stream_relay::RelaySinkError;
use crate::services::discord::outbound::turn_output_controller as toc;

/// Classify a short-replace controller outcome that did not confirm any POST.
/// Confirmed `FreshDelivered` outcomes are success-side terminal resolutions and
/// must never enter this error classifier.
pub(super) fn short_replace_non_delivery_error(outcome: &toc::DeliveryOutcome) -> RelaySinkError {
    debug_assert!(
        !matches!(outcome, toc::DeliveryOutcome::FreshDelivered { .. }),
        "confirmed fresh delivery must stay outside the sink-error taxonomy"
    );
    RelaySinkError::Transient(
        "session-bound short-replace controller delivery not confirmed".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genuinely_uncommitted_short_replace_outcomes_stay_retriable() {
        for uncommitted in [
            toc::DeliveryOutcome::Transient {
                retry_from_offset: 0,
            },
            toc::DeliveryOutcome::Unknown { fell_back: false },
            toc::DeliveryOutcome::Skipped,
        ] {
            let err = short_replace_non_delivery_error(&uncommitted);
            assert!(
                matches!(err, RelaySinkError::Transient(_)),
                "a genuinely-uncommitted controller outcome must stay retriable Transient, got {err:?}"
            );
        }
    }
}
