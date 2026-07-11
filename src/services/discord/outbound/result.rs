//! New delivery outcome enum (#1006 v3).
//!
//! Delivery outcome enum for v3 outbound delivery:
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
//! - `TransientFailure` — no authoritative rejection was observed; the
//!   durable caller retains retry authority.
//!
//! [`super::delivery`] constructs these variants for all outbound callsites.

use poise::serenity_prelude::{ChannelId, MessageId};
use serde::{Deserialize, Serialize};

use super::message::OutboundDedupKey;

/// Tag describing which fallback path a [`DeliveryResult::Fallback`]
/// outcome took.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FallbackUsed {
    /// Primary send returned a length error and a caller-provided minimal
    /// fallback body was posted instead.
    MinimalFallback,
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
    /// Raw id returned by the transport. Production Discord ids are numeric
    /// snowflakes; legacy test doubles also use stable string ids.
    pub(crate) raw_message_id: String,
    /// Present for split delivery. `None` means the message is not a split
    /// chunk, such as compact or file fallback delivery.
    pub(crate) chunk_index: Option<usize>,
    pub(crate) chunk_count: Option<usize>,
}

impl DeliveredMessage {
    // #3034: #1006 v3 outbound result constructors — serde-wire DTO helpers not
    // yet used by every prod path (which builds via `single_raw`).
    #[allow(dead_code)]
    pub(crate) fn single(channel_id: ChannelId, message_id: MessageId) -> Self {
        Self {
            channel_id,
            message_id,
            raw_message_id: message_id.get().to_string(),
            chunk_index: None,
            chunk_count: None,
        }
    }

    pub(crate) fn single_raw(channel_id: ChannelId, raw_message_id: impl Into<String>) -> Self {
        let raw_message_id = raw_message_id.into();
        Self {
            channel_id,
            message_id: parse_message_id_lossy(&raw_message_id),
            raw_message_id,
            chunk_index: None,
            chunk_count: None,
        }
    }

    #[allow(dead_code)] // #3034: #1006 v3 result constructor, see note above.
    pub(crate) fn chunk(
        channel_id: ChannelId,
        message_id: MessageId,
        chunk_index: usize,
        chunk_count: usize,
    ) -> Self {
        Self {
            channel_id,
            message_id,
            raw_message_id: message_id.get().to_string(),
            chunk_index: Some(chunk_index),
            chunk_count: Some(chunk_count),
        }
    }

    pub(crate) fn chunk_raw(
        channel_id: ChannelId,
        raw_message_id: impl Into<String>,
        chunk_index: usize,
        chunk_count: usize,
    ) -> Self {
        let raw_message_id = raw_message_id.into();
        Self {
            channel_id,
            message_id: parse_message_id_lossy(&raw_message_id),
            raw_message_id,
            chunk_index: Some(chunk_index),
            chunk_count: Some(chunk_count),
        }
    }
}

fn parse_message_id_lossy(raw_message_id: &str) -> MessageId {
    raw_message_id
        .parse::<u64>()
        .map(MessageId::new)
        .unwrap_or_else(|_| MessageId::new(1))
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
    /// Discord authoritatively reported that an edit target does not exist
    /// (`404` + code `10008`). This is the only outcome that permits a durable
    /// authority to replace the message with a new nonce/revision.
    ConfirmedMissing { reason: String },
    /// Retryable transport/Discord failure (for example no HTTP response,
    /// 408/429, or 5xx). Durable callers must not collapse this into a
    /// terminal rejection.
    TransientFailure { reason: String },
    /// Terminal failure; the caller should not retry.
    PermanentFailure { reason: String },
}
