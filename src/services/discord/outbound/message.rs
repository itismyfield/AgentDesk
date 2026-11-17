//! Serenity-addressed outbound request envelope and its conversion into the
//! `domain::outbound` key values; Serenity ids stop at this adapter.

use std::path::PathBuf;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use serde::{Deserialize, Serialize};

use super::policy::DiscordOutboundPolicy;
use crate::domain::outbound::ids::{OutboundChannelId, OutboundMessageId, OutboundUserId};
use crate::domain::outbound::intent::OutboundIntent;
pub(crate) use crate::domain::outbound::key::{OutboundDedupKey, OutboundDeliveryId};
use crate::domain::outbound::key::{OutboundOperationKey, OutboundTargetKey};
use crate::services::discord::id_bridge::conversation_from_discord;

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

impl From<OutboundTarget> for OutboundTargetKey {
    fn from(target: OutboundTarget) -> Self {
        match target {
            OutboundTarget::Channel(channel) => Self::Channel {
                channel: channel.into(),
            },
            OutboundTarget::Thread { parent, thread } => Self::Thread {
                parent: parent.into(),
                thread: thread.into(),
            },
            OutboundTarget::DmUser(user) => Self::DmUser { user: user.into() },
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

impl From<OutboundOperation> for OutboundOperationKey {
    fn from(operation: OutboundOperation) -> Self {
        match operation {
            OutboundOperation::Send => Self::Send,
            OutboundOperation::Edit { message_id } => Self::Edit {
                message_id: message_id.into(),
            },
        }
    }
}

impl From<ChannelId> for OutboundChannelId {
    fn from(id: ChannelId) -> Self {
        Self::new(conversation_from_discord(id).get())
    }
}

impl From<MessageId> for OutboundMessageId {
    fn from(id: MessageId) -> Self {
        Self::new(id.get())
    }
}

impl From<UserId> for OutboundUserId {
    fn from(id: UserId) -> Self {
        Self::new(id.get())
    }
}

impl From<OutboundIntent> for DiscordOutboundMessage {
    fn from(intent: OutboundIntent) -> Self {
        let target = match intent.target {
            OutboundTargetKey::Channel { channel } => {
                OutboundTarget::Channel(ChannelId::new(channel.get()))
            }
            OutboundTargetKey::Thread { parent, thread } => OutboundTarget::Thread {
                parent: ChannelId::new(parent.get()),
                thread: ChannelId::new(thread.get()),
            },
            OutboundTargetKey::DmUser { user } => OutboundTarget::DmUser(UserId::new(user.get())),
        };
        let operation = match intent.operation {
            OutboundOperationKey::Send => OutboundOperation::Send,
            OutboundOperationKey::Edit { message_id } => OutboundOperation::Edit {
                message_id: MessageId::new(message_id.get()),
            },
        };
        let mut message = Self::new(
            intent.idempotency.correlation_id,
            intent.idempotency.semantic_event_id,
            intent.content,
            target,
            intent.policy,
        );
        message.operation = operation;
        message
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

    /// Structured dedup key derived from idempotency + target + operation.
    pub(crate) fn dedup_key(&self) -> OutboundDedupKey {
        self.idempotency
            .key_for(self.target.into(), self.operation.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_target_and_reference_wire_stays_snowflake_strings() {
        for raw in [1, 42, u64::MAX] {
            for (target, expected_wire) in [
                (
                    OutboundTarget::Channel(ChannelId::new(raw)),
                    serde_json::json!({"kind": "channel", "ids": raw.to_string()}),
                ),
                (
                    OutboundTarget::Thread {
                        parent: ChannelId::new(7),
                        thread: ChannelId::new(raw),
                    },
                    serde_json::json!({"kind": "thread", "ids": {"parent": "7", "thread": raw.to_string()}}),
                ),
                (
                    OutboundTarget::DmUser(UserId::new(raw)),
                    serde_json::json!({"kind": "dm_user", "ids": raw.to_string()}),
                ),
            ] {
                assert_eq!(serde_json::to_value(target).unwrap(), expected_wire);
                let intent = OutboundIntent::new(
                    "correlation",
                    "event",
                    "body",
                    target.into(),
                    DiscordOutboundPolicy::default(),
                );
                let message = DiscordOutboundMessage::from(intent);
                assert_eq!(message.target, target);
            }
            let reference = OutboundMessageReference {
                channel_id: ChannelId::new(raw),
                message_id: MessageId::new(raw),
            };
            assert_eq!(
                serde_json::to_value(reference).unwrap(),
                serde_json::json!({"channel_id": raw.to_string(), "message_id": raw.to_string()})
            );
            let integer_fixture = serde_json::json!({"channel_id": raw, "message_id": raw});
            let restored: OutboundMessageReference =
                serde_json::from_value(integer_fixture).unwrap();
            assert_eq!(restored, reference);
        }
    }

    #[test]
    fn legacy_u64_channel_binding_still_builds_postgres_arguments() {
        use sqlx::{Arguments, Execute};
        for raw in [1, i64::MAX as u64, u64::MAX] {
            let channel_id: u64 = ChannelId::new(raw).get();
            let mut query =
                sqlx::query::<sqlx::Postgres>("SELECT $1::BIGINT").bind(channel_id as i64);
            assert_eq!(query.take_arguments().unwrap().unwrap().len(), 1);
            assert_eq!(channel_id, raw);
        }
    }

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
