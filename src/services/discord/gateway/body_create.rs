//! Wire adapter shared by the ordinary and direct-TUI gateway implementations.
use super::*;

pub(super) async fn post(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel: ChannelId,
    text: &str,
    nonce: &str,
    reference: Option<(u64, u64)>,
) -> Result<MessageId, String> {
    if reference.is_some_and(|(channel, message)| channel == 0 || message == 0) {
        return Err("invalid body reference".into());
    }
    let id = post_serenity_message_with_nonce(
        http,
        shared,
        channel,
        text,
        reference.map(|(channel, message)| (ChannelId::new(channel), MessageId::new(message))),
        nonce,
        true,
    )
    .await
    .map_err(|e| e.to_string())?;
    id.parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .map(MessageId::new)
        .ok_or("invalid body create ID".into())
}

pub(super) async fn verify(
    http: &serenity::Http,
    channel: ChannelId,
    id: MessageId,
    text: &str,
    reference: Option<(u64, u64)>,
) -> Result<(), String> {
    let own = http.get_current_user().await.map_err(|e| e.to_string())?;
    let message = channel.message(http, id).await.map_err(|e| e.to_string())?;
    let actual_reference = message
        .message_reference
        .as_ref()
        .and_then(|r| r.message_id.map(|m| (r.channel_id.get(), m.get())));
    if message.author.id != own.id
        || message.channel_id != channel
        || message.content != text
        || actual_reference != reference
    {
        return Err(
            "accepted body ID failed author/channel/frozen payload/reference verification".into(),
        );
    }
    Ok(())
}
