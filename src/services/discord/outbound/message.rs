//! New outbound message domain type (#1006 v3, slice 1.0 — types only).
//!
//! This is the outbound message envelope used by v3 delivery. It pairs
//! message content with an explicit
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

// #3034: #1006 v3 outbound producer builder — serde-wire DTO API, not yet
// adopted by every prod callsite. Kept as a coherent builder surface.
#[allow(dead_code)]
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

    // #3034: #1006 v3 serde-wire reference-context builders — not yet wired by
    // every prod callsite; kept as a coherent builder surface.
    #[allow(dead_code)]
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

    #[allow(dead_code)] // #3034: #1006 v3 builder, see note above.
    pub(crate) fn with_thread_name_hint(mut self, thread_name_hint: impl Into<String>) -> Self {
        self.thread_name_hint = Some(thread_name_hint.into());
        self
    }

    #[allow(dead_code)] // #3034: #1006 v3 builder, see note above.
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

// #3034: #1006 v3 outbound target accessors — serde-wire DTO helpers not yet
// consumed by prod delivery (which matches on the target directly).
#[allow(dead_code)]
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
    /// Optional Discord create-message nonce. When `enforce_nonce` is true,
    /// Discord can return the already-created message for a same-author replay
    /// within its bounded nonce-replay window. This is used only by
    /// single-message durable authorities (#4055).
    #[serde(default)]
    pub(crate) create_nonce: Option<String>,
    #[serde(default)]
    pub(crate) enforce_nonce: bool,
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
            create_nonce: None,
            enforce_nonce: false,
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

    pub(crate) fn with_create_nonce(
        mut self,
        nonce: impl Into<String>,
        enforce_nonce: bool,
    ) -> Self {
        self.create_nonce = Some(nonce.into());
        self.enforce_nonce = enforce_nonce;
        self
    }

    // #3034: #1006 v3 envelope builders not yet adopted by every prod callsite;
    // kept as a coherent serde-wire builder API.
    #[allow(dead_code)]
    pub(crate) fn with_producer(mut self, producer: OutboundProducer) -> Self {
        self.producer = Some(producer);
        self
    }

    #[allow(dead_code)] // #3034: #1006 v3 builder, see note above.
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

    #[allow(dead_code)] // #3034: #1006 v3 builder, see note above.
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

    #[allow(dead_code)] // #3034: #1006 v3 builder, see note above.
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

    #[test]
    fn legacy_outbound_envelope_defaults_create_nonce_fields() {
        let message = DiscordOutboundMessage::new(
            "legacy",
            "legacy:event",
            "body",
            OutboundTarget::Channel(ChannelId::new(4055)),
            DiscordOutboundPolicy::default(),
        );
        let mut value = serde_json::to_value(message).expect("serialize outbound envelope");
        let object = value.as_object_mut().expect("outbound envelope object");
        object.remove("create_nonce");
        object.remove("enforce_nonce");

        let restored: DiscordOutboundMessage =
            serde_json::from_value(value).expect("deserialize legacy outbound envelope");
        assert_eq!(restored.create_nonce, None);
        assert!(!restored.enforce_nonce);
    }
}
