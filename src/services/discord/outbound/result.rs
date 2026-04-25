//! New delivery outcome enum (#1006 v3, slice 1.0 — types only).
//!
//! Replaces the legacy [`super::legacy::DeliveryResult`] with a cleaner
//! variant set:
//!
//! - `Sent` — primary delivery succeeded; `messages` identifies every
//!   created Discord message/chunk.
//! - `Fallback` — delivery succeeded via a degraded path (e.g. parent
//!   channel after a thread failure, or a truncated payload). Carries the
//!   reason and the [`FallbackUsed`] tag so observability can attribute
//!   the degradation.
//! - `Skip` — delivery was intentionally skipped (e.g. caller-side
//!   precondition not met). Distinct from `Duplicate`, which is reserved
//!   for the dedup short-circuit.
//! - `Duplicate` — the dedup store already held the structured outbound
//!   dedup key; the previously observed messages are returned so the caller
//!   can replay split/chunk identity.
//! - `PermanentFailure` — delivery failed and must not be retried. The
//!   `reason` string is intended for logs / dead-letter queues.
//!
//! Slice 1.0 is types only — no production code constructs these variants
//! yet. The deliver impl in slice 1.1 will be responsible for choosing the
//! correct variant per outcome.

use poise::serenity_prelude::{ChannelId, MessageId};
use serde::{Deserialize, Serialize};

use super::message::OutboundDedupKey;

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

/// One Discord message produced by an outbound delivery operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeliveredMessage {
    pub(crate) channel_id: ChannelId,
    pub(crate) message_id: MessageId,
    /// Present for split delivery. `None` means the message is not a split
    /// chunk, such as compact or file fallback delivery.
    pub(crate) chunk_index: Option<usize>,
    pub(crate) chunk_count: Option<usize>,
}

impl DeliveredMessage {
    pub(crate) fn single(channel_id: ChannelId, message_id: MessageId) -> Self {
        Self {
            channel_id,
            message_id,
            chunk_index: None,
            chunk_count: None,
        }
    }

    pub(crate) fn chunk(
        channel_id: ChannelId,
        message_id: MessageId,
        chunk_index: usize,
        chunk_count: usize,
    ) -> Self {
        Self {
            channel_id,
            message_id,
            chunk_index: Some(chunk_index),
            chunk_count: Some(chunk_count),
        }
    }
}

/// Outcome of a single outbound delivery attempt (#1006 v3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum DeliveryResult {
    /// Successfully delivered along the primary path.
    Sent {
        dedup_key: OutboundDedupKey,
        messages: Vec<DeliveredMessage>,
    },
    /// Delivered, but via a degraded path. `reason` is human-readable;
    /// `fallback_used` is the structured tag for metrics / dashboards.
    Fallback {
        dedup_key: OutboundDedupKey,
        messages: Vec<DeliveredMessage>,
        reason: String,
        fallback_used: FallbackUsed,
    },
    /// Caller-side precondition skipped the delivery before any send.
    Skip { reason: String },
    /// Dedup short-circuit — the prior structured key already produced these
    /// message ids.
    Duplicate {
        dedup_key: OutboundDedupKey,
        existing_messages: Vec<DeliveredMessage>,
    },
    /// Terminal failure; the caller should not retry.
    PermanentFailure { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::outbound::message::{OutboundOperationKey, OutboundTargetKey};

    fn sample_key() -> OutboundDedupKey {
        OutboundDedupKey {
            correlation_id: "dispatch:1".into(),
            semantic_event_id: "dispatch:1:posted".into(),
            target: OutboundTargetKey::Channel {
                channel: ChannelId::new(10),
            },
            operation: OutboundOperationKey::Send,
        }
    }

    #[test]
    fn sent_construction_and_equality() {
        let a = DeliveryResult::Sent {
            dedup_key: sample_key(),
            messages: vec![DeliveredMessage::single(
                ChannelId::new(10),
                MessageId::new(123),
            )],
        };
        let b = DeliveryResult::Sent {
            dedup_key: sample_key(),
            messages: vec![DeliveredMessage::single(
                ChannelId::new(10),
                MessageId::new(123),
            )],
        };
        assert_eq!(a, b);
    }

    #[test]
    fn sent_can_represent_split_delivery_chunks() {
        let result = DeliveryResult::Sent {
            dedup_key: sample_key(),
            messages: vec![
                DeliveredMessage::chunk(ChannelId::new(10), MessageId::new(1), 0, 2),
                DeliveredMessage::chunk(ChannelId::new(10), MessageId::new(2), 1, 2),
            ],
        };

        match result {
            DeliveryResult::Sent { messages, .. } => {
                assert_eq!(messages.len(), 2);
                assert_eq!(messages[0].chunk_index, Some(0));
                assert_eq!(messages[1].chunk_count, Some(2));
            }
            other => panic!("expected Sent, got {other:?}"),
        }
    }

    #[test]
    fn fallback_carries_reason_and_tag() {
        let result = DeliveryResult::Fallback {
            dedup_key: sample_key(),
            messages: vec![DeliveredMessage::single(
                ChannelId::new(10),
                MessageId::new(7),
            )],
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
            dedup_key: sample_key(),
            existing_messages: vec![DeliveredMessage::single(
                ChannelId::new(10),
                MessageId::new(99),
            )],
        };
        assert_ne!(skip, dup);
    }

    #[test]
    fn duplicate_replays_split_delivery_identity() {
        let key = sample_key();
        let result = DeliveryResult::Duplicate {
            dedup_key: key.clone(),
            existing_messages: vec![
                DeliveredMessage::chunk(ChannelId::new(10), MessageId::new(1), 0, 2),
                DeliveredMessage::chunk(ChannelId::new(10), MessageId::new(2), 1, 2),
            ],
        };

        match result {
            DeliveryResult::Duplicate {
                dedup_key,
                existing_messages,
            } => {
                assert_eq!(dedup_key, key);
                assert_eq!(existing_messages.len(), 2);
                assert_eq!(existing_messages[1].chunk_index, Some(1));
            }
            other => panic!("expected Duplicate, got {other:?}"),
        }
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
            dedup_key: sample_key(),
            existing_messages: vec![DeliveredMessage::single(
                ChannelId::new(10),
                MessageId::new(42),
            )],
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
