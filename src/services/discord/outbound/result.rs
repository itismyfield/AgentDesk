//! Serenity conversion for the outbound result values owned by `domain::outbound::result`.

use poise::serenity_prelude::{ChannelId, Message};

use crate::domain::conversation::{ConversationId, MessageRef};
use crate::services::discord::id_bridge::conversation_from_discord;

use crate::domain::outbound::result::DiscordTransportReceipt;

pub(crate) fn transport_receipt_from_message(
    requested: ChannelId,
    message: &Message,
) -> DiscordTransportReceipt {
    transport_receipt_from_refs(
        conversation_from_discord(requested),
        MessageRef {
            conversation: conversation_from_discord(message.channel_id),
            raw: message.id.get().to_string(),
        },
    )
}

pub(crate) fn transport_receipt_from_refs(
    requested: ConversationId,
    returned: MessageRef,
) -> DiscordTransportReceipt {
    DiscordTransportReceipt {
        requested_channel_id: requested.get().to_string(),
        returned_channel_id: returned.conversation.get().to_string(),
        message_id: returned.raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::conversation::{ConversationId, MessageRef};

    #[test]
    fn receipt_keeps_requested_and_server_returned_identity_separate() {
        let message: Message = serde_json::from_value(serde_json::json!({
            "id": "303", "channel_id": "202", "author": {"id": "1", "username": "bot"},
            "content": "edited", "timestamp": "2026-11-16T00:00:00Z", "tts": false,
            "mention_everyone": false, "mentions": [], "mention_roles": [],
            "attachments": [], "embeds": [], "pinned": false, "type": 0
        }))
        .unwrap();
        let receipt = transport_receipt_from_message(ChannelId::new(101), &message);
        assert_eq!(receipt.requested_channel_id, "101");
        assert_eq!(receipt.returned_channel_id, "202");
        assert_eq!(receipt.message_id, "303");
        for raw in ["1712345678.000001", "opaque/α:00042", "00042", ""] {
            let receipt = transport_receipt_from_refs(
                ConversationId::new(101),
                MessageRef {
                    conversation: ConversationId::new(202),
                    raw: raw.to_owned(),
                },
            );
            assert_eq!(receipt.requested_channel_id, "101");
            assert_eq!(receipt.returned_channel_id, "202");
            assert_eq!(receipt.message_id, raw);
        }
    }
}
