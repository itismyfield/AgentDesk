//! Stable API rendering for scheduled definitions and delivery attempts.

use serde_json::{Value as JsonValue, json};

use super::{DeliveryRow, ScheduledMessageRow};

impl ScheduledMessageRow {
    pub fn to_api_json(&self) -> JsonValue {
        json!({
            "id": self.id,
            "content": self.content,
            "discordMentionUserIds": self.discord_mention_user_ids,
            "title": self.title,
            "targetChannelId": self.target_channel_id,
            "bot": self.bot,
            "deliveryKind": self.delivery_kind,
            "agentId": self.agent_id,
            "agentInstruction": self.agent_instruction,
            "onAgentFailure": self.on_agent_failure,
            "scheduledAt": self.scheduled_at.to_rfc3339(),
            "schedule": self.schedule,
            "timezone": self.timezone,
            "expiresAt": self.expires_at.map(|value| value.to_rfc3339()),
            "status": self.status,
            "inFlightDeliveryId": self.in_flight_delivery_id,
            "fireCount": self.fire_count,
            "lastFiredAt": self.last_fired_at.map(|value| value.to_rfc3339()),
            "lastError": self.last_error,
            "source": self.source,
            "createdBy": self.created_by,
            "dedupeKey": self.dedupe_key,
            "providerTargets": self.provider_target_summary,
            "contextStrategy": self.context_strategy,
            "contextSnapshotId": self.context_snapshot_id,
            "onContextFailure": self.on_context_failure,
            "createdAt": self.created_at.to_rfc3339(),
            "updatedAt": self.updated_at.to_rfc3339(),
        })
    }
}

impl DeliveryRow {
    pub fn to_api_json(&self) -> JsonValue {
        json!({
            "id": self.id,
            "scheduledMessageId": self.scheduled_message_id,
            "fireScheduledAt": self.fire_scheduled_at.to_rfc3339(),
            "deliveryKind": self.delivery_kind,
            "status": self.status,
            "claimOwner": self.claim_owner,
            "outboxId": self.outbox_id,
            "turnId": self.turn_id,
            "fallbackOutboxId": self.fallback_outbox_id,
            "retryCount": self.retry_count,
            "error": self.error,
            "startedAt": self.started_at.to_rfc3339(),
            "finishedAt": self.finished_at.map(|value| value.to_rfc3339()),
            "createdAt": self.created_at.to_rfc3339(),
        })
    }
}
