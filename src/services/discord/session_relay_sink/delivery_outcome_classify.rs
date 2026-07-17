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
/// (`SendFresh` is not this stage's short-replace plan). It maps to `Permanent` as a
/// non-retry INTENT marker, mirroring the control site
/// `tmux_watcher/terminal_send.rs`, which maps `FreshDelivered` to the conservative
/// non-retry `WatcherShortReplaceResult::Skipped`. Every other non-delivery here
/// (ambiguous `PartialContinuationFailure` / transport Err, lost-acquire `Transient`,
/// empty-body `Skipped`) is genuinely uncommitted (offset NOT advanced) → retriable
/// `Transient`.
///
/// IMPORTANT — this mapping does NOT by itself prevent a duplicate POST. The current
/// sink consumer is error-variant-blind: `stream_relay.rs::deliver_frame` folds both
/// `Transient` and `Permanent` into the same sink-error marker, and §3.2 reconciliation
/// (`session_bound_ack.rs`) re-POSTs via SendFull whenever `committed < end` regardless
/// of the error variant. (The original P2 diagnosis — that `Transient` triggers a blind
/// retry — was inaccurate; the real duplicate vector is the §3.2 `committed < end`
/// SendFull.) The arm is dormant today (no `SendFresh` producer), so this is intent-only
/// documentation until the S1r-2~5 cutover guarantees `committed == end` or teaches the
/// consumer to honor the error variant. Tracked in issue #4623.
pub(super) fn short_replace_non_delivery_error(outcome: &toc::DeliveryOutcome) -> RelaySinkError {
    match outcome {
        // Confirmed cross-verb POST → non-retry INTENT marker (`Permanent`). This is a
        // classification-level intent only; it does NOT prevent a duplicate POST on its
        // own (the sink consumer is error-variant-blind — see the fn doc and #4623).
        toc::DeliveryOutcome::FreshDelivered { .. } => RelaySinkError::Permanent(
            "session-bound short-replace controller returned cross-verb FreshDelivered \
             (unreachable this stage); non-retry intent marker (see #4623)"
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

    /// #4046 S1r-1 P2 mutation guard (CLASSIFICATION level): a confirmed cross-verb
    /// `FreshDelivered` outcome must be classified as the non-retry INTENT marker
    /// `RelaySinkError::Permanent`, never retriable `Transient`, mirroring the control
    /// site `tmux_watcher/terminal_send.rs` (`FreshDelivered` →
    /// `WatcherShortReplaceResult::Skipped`). Reverting the `FreshDelivered` arm of
    /// `short_replace_non_delivery_error` to `Transient` FAILS this assert.
    ///
    /// This guards the CLASSIFICATION only. It does NOT prove end-to-end duplicate-POST
    /// prevention: the current sink consumer is error-variant-blind and §3.2
    /// reconciliation re-POSTs on `committed < end` regardless of variant (see the fn
    /// doc). An end-to-end call-site guard is only possible once a `SendFresh` producer
    /// exists at cutover (dormant today, zero producer) — tracked in #4623.
    #[test]
    fn fresh_delivered_short_replace_outcome_is_not_retriable() {
        // committed + persistence_recorded=false is the dedup-less variant the classifier
        // must still mark non-retry.
        let fresh = toc::DeliveryOutcome::FreshDelivered {
            committed_to: Some(7),
            persistence_recorded: false,
        };
        let err = short_replace_non_delivery_error(&fresh);
        assert!(
            matches!(err, RelaySinkError::Permanent(_)),
            "FreshDelivered must be classified as the non-retry Permanent marker, got {err:?}"
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
