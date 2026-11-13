//! Serenity conversion for the outbound result values owned by `domain::outbound::result`.

use poise::serenity_prelude::{ChannelId, Message};

pub(crate) use crate::domain::outbound::result::{
    DeliveredMessage, DeliveryResult, DiscordTransportReceipt, FallbackUsed,
};

pub(crate) fn transport_receipt_from_message(
    requested: ChannelId,
    message: &Message,
) -> DiscordTransportReceipt {
    DiscordTransportReceipt {
        requested_channel_id: requested.get().to_string(),
        returned_channel_id: message.channel_id.get().to_string(),
        message_id: message.id.get().to_string(),
    }
}
