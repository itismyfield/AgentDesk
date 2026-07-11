use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use crate::services::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};

use super::delivery::{deliver_outbound, first_raw_message_id};
use super::message::{DiscordOutboundMessage, OutboundReferenceContext, OutboundTarget};
use super::policy::DiscordOutboundPolicy;
use super::result::DeliveryResult;
use super::{DiscordOutboundClient, shared_outbound_deduper};
use crate::services::discord::{SharedData, rate_limit_wait};

struct SerenityReferenceOutboundClient {
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
}

pub(in crate::services::discord) async fn send_referenced_lifecycle_notice(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    reference_message_id: MessageId,
    content: &str,
    correlation_id: String,
    semantic_event_id: String,
) -> Result<Option<MessageId>, String> {
    let client = SerenityReferenceOutboundClient { http, shared };
    let msg = DiscordOutboundMessage::new(
        correlation_id,
        semantic_event_id,
        content,
        OutboundTarget::Channel(channel_id),
        DiscordOutboundPolicy::preserve_inline_content(),
    )
    .with_reference(OutboundReferenceContext::reply_to(
        channel_id,
        reference_message_id,
    ));
    delivery_message_id(deliver_outbound(&client, shared_outbound_deduper(), msg, None).await)
}

fn delivery_message_id(result: DeliveryResult) -> Result<Option<MessageId>, String> {
    match result {
        DeliveryResult::Sent { messages, .. } => first_raw_message_id(&messages)
            .as_deref()
            .map(parse_message_id)
            .transpose(),
        DeliveryResult::Duplicate {
            existing_messages, ..
        } => first_raw_message_id(&existing_messages)
            .as_deref()
            .map(parse_message_id)
            .transpose(),
        DeliveryResult::Fallback { messages, .. } => first_raw_message_id(&messages)
            .as_deref()
            .map(parse_message_id)
            .transpose(),
        DeliveryResult::Skip { .. } => Ok(None),
        DeliveryResult::TransientFailure { reason }
        | DeliveryResult::PermanentFailure { reason }
        | DeliveryResult::ConfirmedMissing { reason } => Err(reason),
    }
}

impl DiscordOutboundClient for SerenityReferenceOutboundClient {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_channel_id(target_channel)?;
        rate_limit_wait(&self.shared, channel_id).await;
        channel_id
            .send_message(
                &self.http,
                serenity::CreateMessage::new()
                    .content(content)
                    .allowed_mentions(super::super::http::relay_allowed_mentions()),
            )
            .await
            .map(|message| message.id.get().to_string())
            .map_err(dispatch_post_error)
    }

    async fn post_message_with_reference(
        &self,
        target_channel: &str,
        content: &str,
        reference_channel: &str,
        reference_message: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let channel_id = parse_channel_id(target_channel)?;
        let reference_channel_id = parse_channel_id(reference_channel)?;
        let reference_message_id = parse_message_id(reference_message).map_err(|error| {
            DispatchMessagePostError::new(DispatchMessagePostErrorKind::Other, error)
        })?;
        rate_limit_wait(&self.shared, channel_id).await;
        channel_id
            .send_message(
                &self.http,
                serenity::CreateMessage::new()
                    .reference_message(
                        serenity::MessageReference::from((
                            reference_channel_id,
                            reference_message_id,
                        ))
                        .fail_if_not_exists(false),
                    )
                    .content(content)
                    .allowed_mentions(super::super::http::relay_allowed_mentions()),
            )
            .await
            .map(|message| message.id.get().to_string())
            .map_err(dispatch_post_error)
    }
}

fn parse_channel_id(raw: &str) -> Result<ChannelId, DispatchMessagePostError> {
    raw.parse::<u64>().map(ChannelId::new).map_err(|error| {
        DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!("invalid Discord channel id {raw}: {error}"),
        )
    })
}

fn parse_message_id(raw: &str) -> Result<MessageId, String> {
    raw.parse::<u64>()
        .map(MessageId::new)
        .map_err(|error| format!("invalid Discord message id {raw}: {error}"))
}

fn dispatch_post_error(error: serenity::Error) -> DispatchMessagePostError {
    let detail = crate::utils::redact::redact_known_secrets(&error.to_string());
    let lowered = detail.to_ascii_lowercase();
    let kind = if detail.contains("BASE_TYPE_MAX_LENGTH")
        || lowered.contains("2000 or fewer in length")
        || lowered.contains("length")
    {
        DispatchMessagePostErrorKind::MessageTooLong
    } else {
        DispatchMessagePostErrorKind::Other
    };
    DispatchMessagePostError::new(kind, detail)
}
