//! #4046 S1r-1 P2: pure classification of a session-bound short-replace controller
//! outcome that did NOT confirm a placeholder edit. Extracted out of the giant,
//! #3016-hot `session_relay_sink.rs` (frozen prod-LoC baseline) so this
//! retry-classification logic stays unit-testable without re-inflating that file.

use crate::services::cluster::stream_relay::RelaySinkError;
use crate::services::discord::outbound::turn_output_controller as toc;

/// Classify a short-replace controller outcome that did NOT confirm a placeholder
/// edit into a sink error, keeping the retry classification pure and unit-testable
/// (the inline `Delivered`/`NotDelivered` arm in
/// `SessionBoundDiscordRelaySink::deliver_short_replace_via_controller` carries
/// metrics/tracing/observability side effects and cannot be exercised in isolation).
///
/// `FreshDelivered` is a CONFIRMED POST reached via an impossible cross-verb path
/// (`SendFresh` is not this stage's short-replace plan). It MUST be non-retriable:
/// surfacing it as `Transient` would let a blind retry duplicate the landed POST
/// (dedup-less when `persistence_recorded == false`). This mirrors the control site
/// `tmux_watcher/terminal_send.rs`, which maps `FreshDelivered` to the conservative
/// non-retry `WatcherShortReplaceResult::Skipped`. Every other non-delivery here
/// (ambiguous `PartialContinuationFailure` / transport Err, lost-acquire `Transient`,
/// empty-body `Skipped`) is genuinely uncommitted (offset NOT advanced) → retriable
/// `Transient`.
pub(super) fn short_replace_non_delivery_error(outcome: &toc::DeliveryOutcome) -> RelaySinkError {
    match outcome {
        // Confirmed cross-verb POST → NON-retriable. NEVER `Transient` (a blind retry
        // would duplicate the POST). Flipping this back to `Transient` re-arms the
        // duplicate-POST booby-trap #4046 S1r-1 P2 closed.
        toc::DeliveryOutcome::FreshDelivered { .. } => RelaySinkError::Permanent(
            "session-bound short-replace controller returned cross-verb FreshDelivered \
             (unreachable this stage); refusing retriable classification to prevent a \
             duplicate POST"
                .to_string(),
        ),
        // Ambiguous / failed / lost-acquire / empty-body: no confirmed POST, offset NOT
        // advanced → retriable.
        _ => RelaySinkError::Transient(
            "session-bound short-replace controller delivery not confirmed".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #4046 S1r-1 P2 mutation guard: a confirmed cross-verb `FreshDelivered` outcome
    /// must be classified NON-retriably (`RelaySinkError::Permanent`), never as
    /// retriable `Transient` — a blind retry of a landed POST would duplicate the
    /// message (dedup-less when `persistence_recorded == false`). This mirrors the
    /// control site `tmux_watcher/terminal_send.rs` (`FreshDelivered` →
    /// `WatcherShortReplaceResult::Skipped`, non-retry). Reverting the `FreshDelivered`
    /// arm of `short_replace_non_delivery_error` back to `Transient` FAILS this assert.
    #[test]
    fn fresh_delivered_short_replace_outcome_is_not_retriable() {
        // committed + persistence_recorded=false is the exact dedup-less duplicate-POST
        // hazard the fix targets.
        let fresh = toc::DeliveryOutcome::FreshDelivered {
            committed_to: Some(7),
            persistence_recorded: false,
        };
        let err = short_replace_non_delivery_error(&fresh);
        assert!(
            matches!(err, RelaySinkError::Permanent(_)),
            "FreshDelivered (confirmed POST) must be non-retriable Permanent, got {err:?}"
        );

        // Genuinely-uncommitted outcomes (offset NOT advanced) stay retriable Transient.
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
