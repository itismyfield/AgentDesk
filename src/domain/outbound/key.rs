//! Caller idempotency identity and the structured outbound dedup key.

use serde::{Deserialize, Serialize};

use super::ids::{OutboundChannelId, OutboundMessageId, OutboundUserId};

/// `correlation_id` groups related attempts; `semantic_event_id` names one event in the group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundDeliveryId {
    pub correlation_id: String,
    pub semantic_event_id: String,
}

impl OutboundDeliveryId {
    pub fn new(correlation_id: impl Into<String>, semantic_event_id: impl Into<String>) -> Self {
        Self {
            correlation_id: correlation_id.into(),
            semantic_event_id: semantic_event_id.into(),
        }
    }

    pub fn key_for(
        &self,
        target: OutboundTargetKey,
        operation: OutboundOperationKey,
    ) -> OutboundDedupKey {
        OutboundDedupKey {
            correlation_id: self.correlation_id.clone(),
            semantic_event_id: self.semantic_event_id.clone(),
            target,
            operation,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ManualOutboundDeliveryId<'a> {
    pub correlation_id: &'a str,
    pub semantic_event_id: &'a str,
}

/// Typed components instead of a joined string, so `("a::b", "c")` and `("a", "b::c")` differ.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutboundDedupKey {
    pub(crate) correlation_id: String,
    pub(crate) semantic_event_id: String,
    pub(crate) target: OutboundTargetKey,
    pub(crate) operation: OutboundOperationKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutboundTargetKey {
    Channel {
        channel: OutboundChannelId,
    },
    Thread {
        parent: OutboundChannelId,
        thread: OutboundChannelId,
    },
    DmUser {
        user: OutboundUserId,
    },
}

impl OutboundTargetKey {
    pub fn channel(id: u64) -> Self {
        Self::Channel {
            channel: OutboundChannelId::new(id),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutboundOperationKey {
    Send,
    Edit { message_id: OutboundMessageId },
}
