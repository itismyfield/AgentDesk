use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateMessage, EditMessage, Message, MessageId};

pub(in crate::services::discord) async fn send_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
) -> serenity::Result<Message> {
    channel_id
        .send_message(http, CreateMessage::new().content(content))
        .await
}

pub(in crate::services::discord) async fn edit_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    content: &str,
) -> serenity::Result<Message> {
    channel_id
        .edit_message(http, message_id, EditMessage::new().content(content))
        .await
}
