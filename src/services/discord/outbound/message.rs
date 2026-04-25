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

use std::path::PathBuf;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use serde::{Deserialize, Serialize};

use super::policy::DiscordOutboundPolicy;

/// Caller-provided semantic identity for outbound idempotency.
///
/// `correlation_id` groups related outbound attempts (for example, all
/// notifications for one dispatch), while `semantic_event_id` identifies the
/// exact event within that group. Future durable dedup stores should key on
/// both values plus target/operation metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboundDeliveryId {
    pub(crate) correlation_id: String,
    pub(crate) semantic_event_id: String,
}

impl OutboundDeliveryId {
    pub(crate) fn new(
        correlation_id: impl Into<String>,
        semantic_event_id: impl Into<String>,
    ) -> Self {
        Self {
            correlation_id: correlation_id.into(),
            semantic_event_id: semantic_event_id.into(),
        }
    }

    /// Structured dedup key derived from semantic identity plus delivery
    /// target/operation metadata.
    pub(crate) fn key_for(
        &self,
        target: OutboundTarget,
        operation: OutboundOperation,
    ) -> OutboundDedupKey {
        OutboundDedupKey {
            correlation_id: self.correlation_id.clone(),
            semantic_event_id: self.semantic_event_id.clone(),
            target: OutboundTargetKey::from(target),
            operation: OutboundOperationKey::from(operation),
        }
    }
}

/// Structured idempotency key for outbound delivery replay detection.
///
/// This deliberately stays as typed components instead of a delimiter-joined
/// string so values like `("a::b", "c")` and `("a", "b::c")` cannot collide.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct OutboundDedupKey {
    pub(crate) correlation_id: String,
    pub(crate) semantic_event_id: String,
    pub(crate) target: OutboundTargetKey,
    pub(crate) operation: OutboundOperationKey,
}

/// Where an outbound delivery should land.
///
/// Encoded as a sum type so callers can never accidentally request a thread
/// send without also pinning the parent channel — the legacy struct used a
/// pair of `Option<String>` fields and that invariant lived in commentary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "ids", rename_all = "snake_case")]
pub(crate) enum OutboundTarget {
    /// Post to a top-level guild text channel.
    Channel(ChannelId),
    /// Post to a thread inside a parent channel. Both ids are required so
    /// fallback policies can re-route to `parent` if the thread is no longer
    /// reachable.
    Thread {
        parent: ChannelId,
        thread: ChannelId,
    },
    /// Send a direct message to a Discord user. The delivery implementation
    /// will resolve/create the DM channel before posting.
    DmUser(UserId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum OutboundTargetKey {
    Channel {
        channel: ChannelId,
    },
    Thread {
        parent: ChannelId,
        thread: ChannelId,
    },
    DmUser {
        user: UserId,
    },
}

impl From<OutboundTarget> for OutboundTargetKey {
    fn from(target: OutboundTarget) -> Self {
        match target {
            OutboundTarget::Channel(channel) => Self::Channel { channel },
            OutboundTarget::Thread { parent, thread } => Self::Thread { parent, thread },
            OutboundTarget::DmUser(user) => Self::DmUser { user },
        }
    }
}

/// Operation requested by an outbound message envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum OutboundOperation {
    Send,
    Edit { message_id: MessageId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum OutboundOperationKey {
    Send,
    Edit { message_id: MessageId },
}

impl From<OutboundOperation> for OutboundOperationKey {
    fn from(operation: OutboundOperation) -> Self {
        match operation {
            OutboundOperation::Send => Self::Send,
            OutboundOperation::Edit { message_id } => Self::Edit { message_id },
        }
    }
}

/// Optional summary to use when policy selects compact delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboundMessageSummary {
    pub(crate) content: String,
}

/// Attachment metadata/input for file fallback delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboundAttachment {
    pub(crate) filename: String,
    pub(crate) content_type: Option<String>,
    pub(crate) source: OutboundAttachmentSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum OutboundAttachmentSource {
    Bytes { data: Vec<u8> },
    Path { path: PathBuf },
}

/// Metadata that identifies where an outbound message came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboundProducer {
    pub(crate) source: String,
    pub(crate) component: Option<String>,
}

impl OutboundProducer {
    pub(crate) fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            component: None,
        }
    }

    pub(crate) fn with_component(mut self, component: impl Into<String>) -> Self {
        self.component = Some(component.into());
        self
    }
}

/// Which Discord bot identity should execute delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum OutboundBotSelector {
    Default,
    Named { name: String },
    TokenHash { token_hash: String },
    ProviderRole { provider: String, role_id: String },
}

impl Default for OutboundBotSelector {
    fn default() -> Self {
        Self::Default
    }
}

/// Optional reply/reference context for sends, edits, interactions, and
/// command replies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboundReferenceContext {
    pub(crate) message: Option<OutboundMessageReference>,
    pub(crate) interaction: Option<OutboundInteractionReference>,
    pub(crate) thread_name_hint: Option<String>,
    pub(crate) metadata: Vec<OutboundMetadataEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboundMessageReference {
    pub(crate) channel_id: ChannelId,
    pub(crate) message_id: MessageId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboundInteractionReference {
    pub(crate) interaction_id: String,
    pub(crate) token_hint: Option<String>,
    pub(crate) ephemeral: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboundMetadataEntry {
    pub(crate) key: String,
    pub(crate) value: String,
}

impl OutboundReferenceContext {
    pub(crate) fn reply_to(channel_id: ChannelId, message_id: MessageId) -> Self {
        Self {
            message: Some(OutboundMessageReference {
                channel_id,
                message_id,
            }),
            interaction: None,
            thread_name_hint: None,
            metadata: Vec::new(),
        }
    }

    pub(crate) fn interaction(
        interaction_id: impl Into<String>,
        token_hint: Option<impl Into<String>>,
        ephemeral: bool,
    ) -> Self {
        Self {
            message: None,
            interaction: Some(OutboundInteractionReference {
                interaction_id: interaction_id.into(),
                token_hint: token_hint.map(Into::into),
                ephemeral,
            }),
            thread_name_hint: None,
            metadata: Vec::new(),
        }
    }

    pub(crate) fn with_thread_name_hint(mut self, thread_name_hint: impl Into<String>) -> Self {
        self.thread_name_hint = Some(thread_name_hint.into());
        self
    }

    pub(crate) fn with_metadata(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.metadata.push(OutboundMetadataEntry {
            key: key.into(),
            value: value.into(),
        });
        self
    }
}

impl OutboundTarget {
    /// Channel id that should receive the actual HTTP POST. For
    /// [`OutboundTarget::Thread`] this is the thread id; threads are
    /// addressed through the same `/channels/{id}/messages` endpoint as
    /// regular channels in the Discord REST API.
    pub(crate) fn delivery_channel(&self) -> Option<ChannelId> {
        match self {
            Self::Channel(channel) => Some(*channel),
            Self::Thread { thread, .. } => Some(*thread),
            Self::DmUser(_) => None,
        }
    }

    /// Parent channel id, if any. Returns `Some` only for thread targets.
    pub(crate) fn parent_channel(&self) -> Option<ChannelId> {
        match self {
            Self::Channel(_) | Self::DmUser(_) => None,
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
    pub(crate) idempotency: OutboundDeliveryId,
    /// Raw message body; length policy is applied by the deliver impl.
    pub(crate) content: String,
    pub(crate) target: OutboundTarget,
    pub(crate) operation: OutboundOperation,
    pub(crate) producer: Option<OutboundProducer>,
    pub(crate) bot: OutboundBotSelector,
    pub(crate) reference: Option<OutboundReferenceContext>,
    pub(crate) summary: Option<OutboundMessageSummary>,
    pub(crate) attachments: Vec<OutboundAttachment>,
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
            idempotency: OutboundDeliveryId::new(correlation_id, semantic_event_id),
            content: content.into(),
            target,
            operation: OutboundOperation::Send,
            producer: None,
            bot: OutboundBotSelector::Default,
            reference: None,
            summary: None,
            attachments: Vec::new(),
            policy,
        }
    }

    pub(crate) fn with_operation(mut self, operation: OutboundOperation) -> Self {
        self.operation = operation;
        self
    }

    pub(crate) fn with_producer(mut self, producer: OutboundProducer) -> Self {
        self.producer = Some(producer);
        self
    }

    pub(crate) fn with_bot(mut self, bot: OutboundBotSelector) -> Self {
        self.bot = bot;
        self
    }

    pub(crate) fn with_reference(mut self, reference: OutboundReferenceContext) -> Self {
        self.reference = Some(reference);
        self
    }

    pub(crate) fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(OutboundMessageSummary {
            content: summary.into(),
        });
        self
    }

    pub(crate) fn with_bytes_attachment(
        mut self,
        filename: impl Into<String>,
        content_type: Option<impl Into<String>>,
        data: impl Into<Vec<u8>>,
    ) -> Self {
        self.attachments.push(OutboundAttachment {
            filename: filename.into(),
            content_type: content_type.map(Into::into),
            source: OutboundAttachmentSource::Bytes { data: data.into() },
        });
        self
    }

    pub(crate) fn with_path_attachment(
        mut self,
        filename: impl Into<String>,
        content_type: Option<impl Into<String>>,
        path: impl Into<PathBuf>,
    ) -> Self {
        self.attachments.push(OutboundAttachment {
            filename: filename.into(),
            content_type: content_type.map(Into::into),
            source: OutboundAttachmentSource::Path { path: path.into() },
        });
        self
    }

    /// Structured dedup key derived from idempotency + target + operation.
    pub(crate) fn dedup_key(&self) -> OutboundDedupKey {
        self.idempotency.key_for(self.target, self.operation)
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
        assert_eq!(target.delivery_channel(), Some(ChannelId::new(42)));
        assert!(target.parent_channel().is_none());
    }

    #[test]
    fn thread_target_routes_to_thread_with_parent_visible() {
        let target = OutboundTarget::Thread {
            parent: ChannelId::new(100),
            thread: ChannelId::new(101),
        };
        assert_eq!(target.delivery_channel(), Some(ChannelId::new(101)));
        assert_eq!(target.parent_channel(), Some(ChannelId::new(100)));
    }

    #[test]
    fn dm_target_has_no_delivery_channel_until_resolved() {
        let target = OutboundTarget::DmUser(UserId::new(7));
        assert_eq!(target.delivery_channel(), None);
        assert!(target.parent_channel().is_none());
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
        assert_eq!(msg.idempotency.correlation_id, "dispatch:7");
        assert_eq!(msg.idempotency.semantic_event_id, "dispatch:7:sent");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.operation, OutboundOperation::Send);
        assert_eq!(
            msg.dedup_key(),
            OutboundDedupKey {
                correlation_id: "dispatch:7".into(),
                semantic_event_id: "dispatch:7:sent".into(),
                target: OutboundTargetKey::Channel {
                    channel: ChannelId::new(1),
                },
                operation: OutboundOperationKey::Send,
            }
        );
    }

    #[test]
    fn delivery_id_exposes_correlation_and_semantic_event_keys() {
        let id = OutboundDeliveryId::new("dispatch:42", "dispatch:42:posted");
        assert_eq!(id.correlation_id, "dispatch:42");
        assert_eq!(id.semantic_event_id, "dispatch:42:posted");
    }

    #[test]
    fn structured_dedup_key_prevents_delimiter_collisions() {
        let target = OutboundTarget::Channel(ChannelId::new(1));
        let operation = OutboundOperation::Send;
        let left = OutboundDeliveryId::new("a::b", "c").key_for(target, operation);
        let right = OutboundDeliveryId::new("a", "b::c").key_for(target, operation);

        assert_ne!(left, right);
    }

    #[test]
    fn dedup_key_includes_target_and_operation_metadata() {
        let base = OutboundDeliveryId::new("dispatch:42", "posted");
        let channel_key = base.key_for(
            OutboundTarget::Channel(ChannelId::new(1)),
            OutboundOperation::Send,
        );
        let thread_key = base.key_for(
            OutboundTarget::Thread {
                parent: ChannelId::new(1),
                thread: ChannelId::new(2),
            },
            OutboundOperation::Send,
        );
        let edit_key = base.key_for(
            OutboundTarget::Channel(ChannelId::new(1)),
            OutboundOperation::Edit {
                message_id: MessageId::new(99),
            },
        );

        assert_ne!(channel_key, thread_key);
        assert_ne!(channel_key, edit_key);
    }

    #[test]
    fn message_can_carry_edit_summary_and_metadata_inputs() {
        let msg = DiscordOutboundMessage::new(
            "dispatch:7",
            "dispatch:7:edit",
            "full",
            OutboundTarget::Channel(ChannelId::new(1)),
            sample_policy(),
        )
        .with_operation(OutboundOperation::Edit {
            message_id: MessageId::new(77),
        })
        .with_producer(OutboundProducer::new("health_api").with_component("send_alias"))
        .with_bot(OutboundBotSelector::Named {
            name: "notify".into(),
        })
        .with_reference(
            OutboundReferenceContext::reply_to(ChannelId::new(1), MessageId::new(55))
                .with_thread_name_hint("support-thread")
                .with_metadata("route", "/api/discord/send"),
        )
        .with_summary("short")
        .with_bytes_attachment("full.txt", Some("text/plain"), b"full".to_vec());

        assert_eq!(
            msg.operation,
            OutboundOperation::Edit {
                message_id: MessageId::new(77),
            }
        );
        assert_eq!(msg.producer.as_ref().unwrap().source, "health_api");
        assert_eq!(
            msg.bot,
            OutboundBotSelector::Named {
                name: "notify".into(),
            }
        );
        let reference = msg.reference.as_ref().unwrap();
        assert_eq!(
            reference.message,
            Some(OutboundMessageReference {
                channel_id: ChannelId::new(1),
                message_id: MessageId::new(55),
            })
        );
        assert_eq!(reference.metadata[0].key, "route");
        assert_eq!(msg.summary.as_ref().unwrap().content, "short");
        assert_eq!(msg.attachments.len(), 1);
    }

    #[test]
    fn attachment_inputs_support_bytes_and_paths() {
        let msg = DiscordOutboundMessage::new(
            "dispatch:7",
            "dispatch:7:files",
            "see files",
            OutboundTarget::Channel(ChannelId::new(1)),
            sample_policy(),
        )
        .with_bytes_attachment("raw.bin", Some("application/octet-stream"), vec![0, 1, 2])
        .with_path_attachment("report.txt", None::<String>, "/tmp/report.txt");

        assert_eq!(msg.attachments.len(), 2);
        assert_eq!(
            msg.attachments[0].source,
            OutboundAttachmentSource::Bytes {
                data: vec![0, 1, 2],
            }
        );
        assert_eq!(
            msg.attachments[1].source,
            OutboundAttachmentSource::Path {
                path: PathBuf::from("/tmp/report.txt"),
            }
        );
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
