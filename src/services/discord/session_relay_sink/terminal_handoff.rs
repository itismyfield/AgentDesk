use async_trait::async_trait;

use super::{SessionBoundDiscordRelaySink, SessionRelayDelivery, delivery_frontier};
use crate::services::cluster::stream_relay::{
    RelaySink, RelaySinkError, RelaySinkOutcome, StreamFrame,
};

/// #3041 P1-5: the SINK-LOCAL terminal outcome stays deliberately 2-way — the sink
/// always KNOWS its result: confirmed POST/edit → `Delivered`; deterministic
/// route decline (foreign-owner block / bridge-owned / mismatched inflight) →
/// `NotDelivered`; transport/format failure → `Err`. NO sink-local `Unknown` (that
/// is the cross-actor relay-ring + watcher state). `NotDelivered` (former `Skipped`)
/// maps to `RelaySinkOutcome::TerminalNotDelivered`, routed through §3.2
/// reconciliation — never a blind skip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionRelayDeliveryOutcome {
    Delivered,
    LandedStale,
    LandedUnrecorded,
    FreshDelivered {
        committed_to: Option<u64>,
        persistence_recorded: bool,
    },
    SentButUncommitted,
    NotDelivered,
}

impl SessionRelayDeliveryOutcome {
    pub(super) fn from_proof(result: delivery_frontier::SinkDeliveryProofResult) -> Self {
        match result {
            delivery_frontier::SinkDeliveryProofResult::Persisted => Self::Delivered,
            delivery_frontier::SinkDeliveryProofResult::LandedStale => Self::LandedStale,
            delivery_frontier::SinkDeliveryProofResult::LandedUnrecorded => Self::LandedUnrecorded,
        }
    }
}

#[async_trait]
impl RelaySink for SessionBoundDiscordRelaySink {
    async fn deliver(&self, frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
        // #3041 P1-3 R5 (codex — REVERT R4 fence-gating of the outcome): a result-bearing
        // delivery reports Delivered/NotDelivered REGARDLESS of a fence on this frame
        // (R4's gate BLACK-HOLED the legitimate no-inflight terminal — no fence but a real
        // terminal → `FrameAccepted` → watcher timed out). The co-chunked confusion is now
        // handled by the per-sequence ACK. The fence still ONLY gates the OFFSET ADVANCE
        // (inline in `deliver_response`) — outcome and advance are decoupled.
        let deliveries = self.ingest_frame(frame);
        let fenced_terminal_without_delivery = deliveries.is_empty()
            && matches!(
                (frame.turn_start_offset, frame.terminal_consumed_end),
                (Some(start), Some(end)) if end > start
            );
        let mut terminal_delivered = false;
        let mut terminal_fresh_delivered = None;
        let mut terminal_not_delivered = false;
        for delivery in deliveries {
            let delivery_outcome = self.deliver_response(delivery).await;
            #[cfg(test)]
            if let (Ok(outcome), Some(outcomes)) =
                (delivery_outcome.as_ref(), &self.test_delivery_outcomes)
            {
                outcomes
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(*outcome);
            }
            match delivery_outcome {
                Ok(SessionRelayDeliveryOutcome::Delivered) => {
                    // #3041 P1-3 (B1 CLOSED): the offset advance is owned INLINE by
                    // `deliver_response` — see `advance_after_confirmed_post`.
                    terminal_delivered = true;
                }
                Ok(
                    SessionRelayDeliveryOutcome::LandedStale
                    | SessionRelayDeliveryOutcome::LandedUnrecorded,
                ) => {
                    // Transport landed, but the captured source authority was
                    // stale or its proof could not be durably recorded. Never
                    // retry this POST into the replacement incarnation.
                    terminal_delivered = true;
                }
                Ok(SessionRelayDeliveryOutcome::FreshDelivered {
                    committed_to,
                    persistence_recorded,
                }) => {
                    terminal_fresh_delivered = Some((committed_to, persistence_recorded));
                }
                Ok(SessionRelayDeliveryOutcome::SentButUncommitted) => {
                    return Ok(RelaySinkOutcome::TerminalUnknown);
                }
                Ok(SessionRelayDeliveryOutcome::NotDelivered) => {
                    terminal_not_delivered = true;
                }
                Err(error) => return Err(error),
            }
        }
        // #3041 P1-3 R5: surface the outcome on THIS frame's sequence (the watcher
        // resolves its own terminal ACK on its exact seq, so a co-chunked tail can't
        // satisfy another turn's ACK). A valid terminal commit fence proves this exact
        // sequence needs a terminal resolution even when parser visibility policy emits
        // no delivery; resolve it as NotDelivered so the watcher reconciles immediately.
        // An unfenced frame with no result-bearing delivery remains `FrameAccepted`.
        // #3041 P1-5: NO `TerminalUnknown` (the sink always KNOWS its result).
        if terminal_delivered {
            Ok(RelaySinkOutcome::TerminalDelivered)
        } else if let Some((committed_to, persistence_recorded)) = terminal_fresh_delivered {
            Ok(RelaySinkOutcome::TerminalFreshDelivered {
                committed_to,
                persistence_recorded,
            })
        } else if terminal_not_delivered || fenced_terminal_without_delivery {
            Ok(RelaySinkOutcome::TerminalNotDelivered)
        } else {
            Ok(RelaySinkOutcome::FrameAccepted)
        }
    }
}
