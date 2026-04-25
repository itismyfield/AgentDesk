//! New delivery outcome enum (#1006 v3, slice 1.0 — types only).
//!
//! Replaces the legacy [`super::legacy::DeliveryResult`] with a cleaner
//! variant set:
//!
//! - `Sent` — primary delivery succeeded; `message_id` identifies the
//!   created Discord message.
//! - `Fallback` — delivery succeeded via a degraded path (e.g. parent
//!   channel after a thread failure, or a truncated payload). Carries the
//!   reason and the [`FallbackUsed`] tag so observability can attribute
//!   the degradation.
//! - `Skip` — delivery was intentionally skipped (e.g. caller-side
//!   precondition not met). Distinct from `Duplicate`, which is reserved
//!   for the dedup short-circuit.
//! - `Duplicate` — the dedup store already held the
//!   `(correlation_id, semantic_event_id)` pair; the previously observed
//!   `existing_message_id` is returned so the caller can reuse it.
//! - `PermanentFailure` — delivery failed and must not be retried. The
//!   `reason` string is intended for logs / dead-letter queues.
//!
//! Slice 1.0 is types only — no production code constructs these variants
//! yet. The deliver impl in slice 1.1 will be responsible for choosing the
//! correct variant per outcome.

use poise::serenity_prelude::MessageId;
use serde::{Deserialize, Serialize};

/// Tag describing which fallback path a [`DeliveryResult::Fallback`]
/// outcome took.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FallbackUsed {
    /// Payload was truncated / compacted to fit the per-message limit.
    LengthCompacted,
    /// Payload was split across multiple messages.
    LengthSplit,
    /// Payload was attached as a file rather than inlined.
    FileAttachment,
    /// Delivery routed to the parent channel after a thread error.
    ParentChannel,
}

/// Outcome of a single outbound delivery attempt (#1006 v3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum DeliveryResult {
    /// Successfully delivered along the primary path.
    Sent { message_id: MessageId },
    /// Delivered, but via a degraded path. `reason` is human-readable;
    /// `fallback_used` is the structured tag for metrics / dashboards.
    Fallback {
        message_id: MessageId,
        reason: String,
        fallback_used: FallbackUsed,
    },
    /// Caller-side precondition skipped the delivery before any send.
    Skip { reason: String },
    /// Dedup short-circuit — the prior `(correlation_id,
    /// semantic_event_id)` pair already produced this message.
    Duplicate { existing_message_id: MessageId },
    /// Terminal failure; the caller should not retry.
    PermanentFailure { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sent_construction_and_equality() {
        let a = DeliveryResult::Sent {
            message_id: MessageId::new(123),
        };
        let b = DeliveryResult::Sent {
            message_id: MessageId::new(123),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn fallback_carries_reason_and_tag() {
        let result = DeliveryResult::Fallback {
            message_id: MessageId::new(7),
            reason: "thread archived".into(),
            fallback_used: FallbackUsed::ParentChannel,
        };
        match result {
            DeliveryResult::Fallback {
                fallback_used,
                reason,
                ..
            } => {
                assert_eq!(fallback_used, FallbackUsed::ParentChannel);
                assert_eq!(reason, "thread archived");
            }
            other => panic!("expected Fallback, got {other:?}"),
        }
    }

    #[test]
    fn skip_and_duplicate_are_distinct_variants() {
        let skip = DeliveryResult::Skip {
            reason: "muted channel".into(),
        };
        let dup = DeliveryResult::Duplicate {
            existing_message_id: MessageId::new(99),
        };
        assert_ne!(skip, dup);
    }

    #[test]
    fn permanent_failure_round_trips_through_serde() {
        let result = DeliveryResult::PermanentFailure {
            reason: "discord 403 forbidden".into(),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let back: DeliveryResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result, back);
    }

    #[test]
    fn duplicate_serde_uses_tagged_kind() {
        let result = DeliveryResult::Duplicate {
            existing_message_id: MessageId::new(42),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(json.contains("\"kind\":\"duplicate\""), "got: {json}");
        let back: DeliveryResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result, back);
    }

    #[test]
    fn fallback_used_serde_roundtrip_all_variants() {
        for variant in [
            FallbackUsed::LengthCompacted,
            FallbackUsed::LengthSplit,
            FallbackUsed::FileAttachment,
            FallbackUsed::ParentChannel,
        ] {
            let json = serde_json::to_string(&variant).expect("serialize");
            let back: FallbackUsed = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(variant, back);
        }
    }
}
