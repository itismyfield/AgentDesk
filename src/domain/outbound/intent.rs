//! One outbound delivery request in Serenity-free values; the adapter converts it to its envelope.

use super::key::{OutboundDeliveryId, OutboundOperationKey, OutboundTargetKey};
use super::policy::DiscordOutboundPolicy;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutboundIntent {
    pub(crate) idempotency: OutboundDeliveryId,
    pub(crate) content: String,
    pub(crate) target: OutboundTargetKey,
    pub(crate) operation: OutboundOperationKey,
    pub(crate) policy: DiscordOutboundPolicy,
}

impl OutboundIntent {
    pub(crate) fn new(
        correlation_id: impl Into<String>,
        semantic_event_id: impl Into<String>,
        content: impl Into<String>,
        target: OutboundTargetKey,
        policy: DiscordOutboundPolicy,
    ) -> Self {
        Self {
            idempotency: OutboundDeliveryId::new(correlation_id, semantic_event_id),
            content: content.into(),
            target,
            operation: OutboundOperationKey::Send,
            policy,
        }
    }

    pub(crate) fn with_operation(mut self, operation: OutboundOperationKey) -> Self {
        self.operation = operation;
        self
    }
}
