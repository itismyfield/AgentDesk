//! New outbound message domain type (#1006 v3, slice 1.0 — types only).
//!
//! This is the v3 replacement for the legacy `DiscordOutboundMessage`
//! defined in [`super::legacy`]. It pairs message content with an explicit
//! [`OutboundTarget`] (so channel-vs-thread routing is encoded in the type
//! system rather than in two parallel `Option<String>` fields) and embeds
//! the per-message [`super::policy::DiscordOutboundPolicy`] so the deliver
//! function can be a free, callsite-agnostic helper.
//!
//! No callsite consumes this type in slice 1.0; the migration happens in
//! slices 1.1 (service impl) and 1.2 (outbox callsite rewire).
//!
//! `serenity::model::id::ChannelId` is a transparent newtype around a `u64`
//! and already implements `Serialize` / `Deserialize`, so this struct can
//! round-trip through serde without manual glue.

use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};

use super::policy::DiscordOutboundPolicy;

/// Where an outbound delivery should land.
///
/// Encoded as a sum type so callers can never accidentally request a thread
/// send without also pinning the parent channel — the legacy struct used a
/// pair of `Option<String>` fields and that invariant lived in commentary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "ids", rename_all = "snake_case")]
pub(crate) enum OutboundTarget {
    /// Post to a top-level guild text channel (or DM channel).
    Channel(ChannelId),
    /// Post to a thread inside a parent channel. Both ids are required so
    /// fallback policies can re-route to `parent` if the thread is no longer
    /// reachable.
    Thread {
        parent: ChannelId,
        thread: ChannelId,
    },
}

impl OutboundTarget {
    /// Channel id that should receive the actual HTTP POST. For
    /// [`OutboundTarget::Thread`] this is the thread id; threads are
    /// addressed through the same `/channels/{id}/messages` endpoint as
    /// regular channels in the Discord REST API.
    pub(crate) fn delivery_channel(&self) -> ChannelId {
        match self {
            Self::Channel(channel) => *channel,
            Self::Thread { thread, .. } => *thread,
        }
    }

    /// Parent channel id, if any. Returns `Some` only for thread targets.
    pub(crate) fn parent_channel(&self) -> Option<ChannelId> {
        match self {
            Self::Channel(_) => None,
            Self::Thread { parent, .. } => Some(*parent),
        }
    }
}

/// New outbound message envelope (#1006 v3).
///
/// Carries the payload, the resolved [`OutboundTarget`], the per-message
/// [`DiscordOutboundPolicy`], and the dedup keys (`correlation_id` +
/// `semantic_event_id`). Both ids are mandatory in the v3 shape — every
/// outbound call must declare its idempotency identity, which lets a
/// future DB-backed deduper key on `(correlation_id, semantic_event_id)`
/// without nullable columns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DiscordOutboundMessage {
    /// Caller-supplied grouping key (e.g. `dispatch:42`, `review:7`).
    pub(crate) correlation_id: String,
    /// Specific event within the correlation group (e.g.
    /// `dispatch:42:sent`, `review:7:pass`).
    pub(crate) semantic_event_id: String,
    /// Raw message body; length policy is applied by the deliver impl.
    pub(crate) content: String,
    pub(crate) target: OutboundTarget,
    pub(crate) policy: DiscordOutboundPolicy,
}

impl DiscordOutboundMessage {
    /// Convenience constructor — keeps callsites short while preserving the
    /// "all fields required" invariant.
    pub(crate) fn new(
        correlation_id: impl Into<String>,
        semantic_event_id: impl Into<String>,
        content: impl Into<String>,
        target: OutboundTarget,
        policy: DiscordOutboundPolicy,
    ) -> Self {
        Self {
            correlation_id: correlation_id.into(),
            semantic_event_id: semantic_event_id.into(),
            content: content.into(),
            target,
            policy,
        }
    }

    /// Composite dedup key derived from `(correlation_id, semantic_event_id)`.
    pub(crate) fn dedup_key(&self) -> String {
        format!("{}::{}", self.correlation_id, self.semantic_event_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::outbound::policy::{
        DiscordOutboundPolicy, FallbackPolicy, LengthStrategy,
    };
    use std::time::Duration;

    fn sample_policy() -> DiscordOutboundPolicy {
        DiscordOutboundPolicy {
            length_strategy: LengthStrategy::Split,
            fallback: FallbackPolicy::None,
            idempotency_window: Duration::from_secs(60),
        }
    }

    #[test]
    fn channel_target_routes_to_self() {
        let target = OutboundTarget::Channel(ChannelId::new(42));
        assert_eq!(target.delivery_channel(), ChannelId::new(42));
        assert!(target.parent_channel().is_none());
    }

    #[test]
    fn thread_target_routes_to_thread_with_parent_visible() {
        let target = OutboundTarget::Thread {
            parent: ChannelId::new(100),
            thread: ChannelId::new(101),
        };
        assert_eq!(target.delivery_channel(), ChannelId::new(101));
        assert_eq!(target.parent_channel(), Some(ChannelId::new(100)));
    }

    #[test]
    fn message_construction_and_dedup_key() {
        let msg = DiscordOutboundMessage::new(
            "dispatch:7",
            "dispatch:7:sent",
            "hello",
            OutboundTarget::Channel(ChannelId::new(1)),
            sample_policy(),
        );
        assert_eq!(msg.correlation_id, "dispatch:7");
        assert_eq!(msg.semantic_event_id, "dispatch:7:sent");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.dedup_key(), "dispatch:7::dispatch:7:sent");
    }

    #[test]
    fn message_equality_is_structural() {
        let target = OutboundTarget::Channel(ChannelId::new(1));
        let a = DiscordOutboundMessage::new("c", "s", "x", target, sample_policy());
        let b = DiscordOutboundMessage::new("c", "s", "x", target, sample_policy());
        assert_eq!(a, b);
    }

    #[test]
    fn message_serde_roundtrips() {
        let msg = DiscordOutboundMessage::new(
            "dispatch:9",
            "dispatch:9:sent",
            "payload",
            OutboundTarget::Thread {
                parent: ChannelId::new(200),
                thread: ChannelId::new(201),
            },
            sample_policy(),
        );
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: DiscordOutboundMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn outbound_target_serde_uses_tagged_form() {
        let channel = OutboundTarget::Channel(ChannelId::new(11));
        let json = serde_json::to_string(&channel).expect("serialize");
        assert!(json.contains("\"kind\":\"channel\""), "got: {json}");
        let back: OutboundTarget = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(channel, back);
    }
}
