//! Explicit Discord conversions keep SDK impl candidates and persisted IDs unchanged.

use std::num::NonZeroU64;

use poise::serenity_prelude::{ChannelId, MessageId};

use crate::domain::conversation::{ConversationId, MessageRef};

#[derive(Debug, thiserror::Error)]
pub(crate) enum IdError {
    #[error("invalid conversation id 0: Discord channels must be non-zero")]
    InvalidConversation,
    #[error("invalid Discord message id {0}: expected a non-zero u64")]
    InvalidMessage(String),
}

pub(crate) fn conversation_from_discord(channel: ChannelId) -> ConversationId {
    ConversationId::new(channel.get())
}

pub(crate) fn discord_channel_from_conversation(
    conversation: ConversationId,
) -> Result<ChannelId, IdError> {
    let raw = NonZeroU64::new(conversation.get()).ok_or(IdError::InvalidConversation)?;
    Ok(ChannelId::new(raw.get()))
}

pub(crate) fn discord_message_from_ref(
    reference: &MessageRef,
) -> Result<(ChannelId, MessageId), IdError> {
    let channel = discord_channel_from_conversation(reference.conversation)?;
    let message = reference
        .raw
        .parse::<NonZeroU64>()
        .map_err(|error| IdError::InvalidMessage(format!("{}: {error}", reference.raw)))?;
    Ok((channel, MessageId::new(message.get())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_roundtrip_keeps_full_u64_identity() {
        for raw in [1, 42, u64::MAX] {
            let channel = ChannelId::new(raw);
            let conversation = conversation_from_discord(channel);
            assert_eq!(conversation.get(), raw);
            assert_eq!(
                discord_channel_from_conversation(conversation).unwrap(),
                channel
            );
            assert_eq!(
                conversation_from_discord(
                    discord_channel_from_conversation(ConversationId::new(raw)).unwrap()
                ),
                ConversationId::new(raw)
            );
        }
    }

    #[test]
    fn zero_conversation_is_rejected_before_discord_construction() {
        assert!(discord_channel_from_conversation(ConversationId(0)).is_err());
        let reference = MessageRef {
            conversation: ConversationId(0),
            raw: "42".to_owned(),
        };
        assert!(discord_message_from_ref(&reference).is_err());
    }

    #[test]
    fn only_discord_boundary_parses_message_ids_without_mutating_raw() {
        for raw in ["00042", "18446744073709551615"] {
            let reference = MessageRef {
                conversation: ConversationId::new(7),
                raw: raw.to_owned(),
            };
            let (channel, message) = discord_message_from_ref(&reference).unwrap();
            assert_eq!(channel.get(), 7);
            assert_eq!(
                message.get().to_string(),
                raw.parse::<u64>().unwrap().to_string()
            );
            assert_eq!(reference.raw, raw);
        }
        for raw in [
            "0",
            "1712345678.000001",
            "opaque/α",
            "",
            "18446744073709551616",
        ] {
            let reference = MessageRef {
                conversation: ConversationId::new(7),
                raw: raw.to_owned(),
            };
            assert!(discord_message_from_ref(&reference).is_err());
            assert_eq!(reference.raw, raw);
        }
    }
}
