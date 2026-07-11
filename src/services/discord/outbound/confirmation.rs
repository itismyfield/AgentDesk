use poise::serenity_prelude::{ChannelId, MessageId};

use super::delivery::{deliver_outbound, first_raw_message_id};
use super::message::OutboundTarget;
use super::{
    DeliveryResult, DiscordOutboundMessage, DiscordOutboundPolicy, HttpOutboundClient,
    shared_outbound_deduper,
};

type ConfirmationResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub(crate) async fn send_command_confirmation_message(
    token: &str,
    channel_id: ChannelId,
    content: impl Into<String>,
) -> ConfirmationResult<MessageId> {
    let client = HttpOutboundClient::new(
        reqwest::Client::new(),
        token.to_string(),
        crate::services::dispatches::discord_delivery::discord_api_base_url(),
    );
    let message = DiscordOutboundMessage::new(
        "discord-command-confirmation",
        "confirmation-posted",
        content.into(),
        OutboundTarget::Channel(channel_id),
        DiscordOutboundPolicy::preserve_inline_content().without_idempotency(),
    );

    let raw_message_id = match deliver_outbound(&client, shared_outbound_deduper(), message, None)
        .await
    {
        DeliveryResult::Sent { messages, .. } | DeliveryResult::Fallback { messages, .. } => {
            first_raw_message_id(&messages)
                .ok_or_else(|| confirmation_error("confirmation delivery returned no message id"))?
        }
        DeliveryResult::Duplicate {
            existing_messages, ..
        } => first_raw_message_id(&existing_messages)
            .ok_or_else(|| confirmation_error("duplicate confirmation without message id"))?,
        DeliveryResult::Skip { reason } => {
            return Err(confirmation_error(format!(
                "confirmation delivery skipped: {reason}"
            )));
        }
        DeliveryResult::TransientFailure { reason }
        | DeliveryResult::PermanentFailure { reason }
        | DeliveryResult::ConfirmedMissing { reason } => {
            return Err(confirmation_error(reason));
        }
    };

    raw_message_id
        .parse::<u64>()
        .map(MessageId::new)
        .map_err(|error| {
            confirmation_error(format!(
                "confirmation delivery returned invalid message id {raw_message_id}: {error}"
            ))
        })
}

fn confirmation_error(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::other(message.into()).into()
}
