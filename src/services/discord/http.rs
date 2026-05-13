use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateActionRow, CreateMessage, EditMessage, Message, MessageId};

pub(in crate::services::discord) async fn send_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
) -> serenity::Result<Message> {
    channel_id
        .send_message(http, CreateMessage::new().content(content))
        .await
}

/// Send a channel message with attached interactive components (buttons,
/// select menus). Lives here so the maintainability audit's
/// `direct_discord_sends` allowlist (which covers `discord/http.rs`)
/// continues to apply to interactive idle-recap cards.
pub(in crate::services::discord) async fn send_channel_message_with_components(
    http: &serenity::Http,
    channel_id: ChannelId,
    content: &str,
    components: Vec<CreateActionRow>,
) -> serenity::Result<Message> {
    channel_id
        .send_message(
            http,
            CreateMessage::new().content(content).components(components),
        )
        .await
}

/// Edit a channel message while replacing its interactive components.
pub(in crate::services::discord) async fn edit_channel_message_with_components(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    content: &str,
    components: Vec<CreateActionRow>,
) -> serenity::Result<Message> {
    channel_id
        .edit_message(
            http,
            message_id,
            EditMessage::new().content(content).components(components),
        )
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

/// Delete a single channel message by id. Errors are propagated; callers
/// that don't care about Discord-side 404s (already deleted) should wrap
/// the call in `let _ =`.
pub(in crate::services::discord) async fn delete_channel_message(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
) -> serenity::Result<()> {
    channel_id.delete_message(http, message_id).await
}
