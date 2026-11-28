//! Outcome of one outbound delivery and the transport receipt of a created message.

use serde::{Deserialize, Serialize};

use super::ids::{OutboundChannelId, OutboundMessageId};
use super::key::OutboundDedupKey;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordTransportReceipt {
    pub requested_channel_id: String,
    pub returned_channel_id: String,
    pub message_id: String,
}

/// Which degraded path a [`DeliveryResult::Fallback`] took.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackUsed {
    MinimalFallback,
    LengthCompacted,
    LengthSplit,
    FileAttachment,
    ParentChannel,
}

/// One Discord message produced by an outbound delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredMessage {
    pub(crate) channel_id: OutboundChannelId,
    pub(crate) message_id: OutboundMessageId,
    #[cfg(any(test, feature = "test-support"))]
    /// Id as returned by the transport; test doubles may return non-numeric ids.
    pub raw_message_id: String,
    #[cfg(not(any(test, feature = "test-support")))]
    /// Id as returned by the transport; test doubles may return non-numeric ids.
    pub(crate) raw_message_id: String,
    #[cfg(any(test, feature = "test-support"))]
    /// `None` for anything that is not a split chunk.
    pub chunk_index: Option<usize>,
    #[cfg(not(any(test, feature = "test-support")))]
    /// `None` for anything that is not a split chunk.
    pub(crate) chunk_index: Option<usize>,
    #[cfg(any(test, feature = "test-support"))]
    pub chunk_count: Option<usize>,
    #[cfg(not(any(test, feature = "test-support")))]
    pub(crate) chunk_count: Option<usize>,
}

impl DeliveredMessage {
    pub fn single_raw(
        channel_id: impl Into<OutboundChannelId>,
        raw_message_id: impl Into<String>,
    ) -> Self {
        let raw_message_id = raw_message_id.into();
        Self {
            channel_id: channel_id.into(),
            message_id: parse_message_id_lossy(&raw_message_id),
            raw_message_id,
            chunk_index: None,
            chunk_count: None,
        }
    }

    pub fn chunk_raw(
        channel_id: impl Into<OutboundChannelId>,
        raw_message_id: impl Into<String>,
        chunk_index: usize,
        chunk_count: usize,
    ) -> Self {
        let raw_message_id = raw_message_id.into();
        Self {
            channel_id: channel_id.into(),
            message_id: parse_message_id_lossy(&raw_message_id),
            raw_message_id,
            chunk_index: Some(chunk_index),
            chunk_count: Some(chunk_count),
        }
    }
}

fn parse_message_id_lossy(raw_message_id: &str) -> OutboundMessageId {
    raw_message_id
        .parse::<u64>()
        .map(OutboundMessageId::new)
        .unwrap_or_else(|_| OutboundMessageId::new(1))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeliveryResult {
    Sent {
        dedup_key: OutboundDedupKey,
        messages: Vec<DeliveredMessage>,
    },
    Fallback {
        dedup_key: OutboundDedupKey,
        messages: Vec<DeliveredMessage>,
        reason: String,
        fallback_used: FallbackUsed,
    },
    /// Caller-side precondition skipped the delivery before any send.
    Skip {
        reason: String,
    },
    /// The dedup key already produced these messages.
    Duplicate {
        dedup_key: OutboundDedupKey,
        existing_messages: Vec<DeliveredMessage>,
    },
    /// Edit target authoritatively missing (404 + 10008); only this permits a new nonce/revision.
    ConfirmedMissing {
        reason: String,
    },
    /// No authoritative rejection was seen; durable callers keep retry authority.
    TransientFailure {
        reason: String,
    },
    PermanentFailure {
        reason: String,
    },
}

pub fn first_raw_message_id(messages: &[DeliveredMessage]) -> Option<String> {
    messages
        .first()
        .map(|message| message.raw_message_id.clone())
}
