use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

pub(in crate::services::discord) async fn cleanup_recovered_catch_up_hourglass(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
) {
    remove_reaction(http, channel_id, message_id, '⏳').await;
}

#[cfg(not(test))]
async fn remove_reaction(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
) {
    super::formatting::remove_reaction_raw(http, channel_id, message_id, emoji).await;
}

#[cfg(test)]
static REACTION_CLEANUP_RECORDS: std::sync::LazyLock<
    std::sync::Mutex<Option<Vec<(u64, u64, char)>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
fn begin_recording() {
    *REACTION_CLEANUP_RECORDS
        .lock()
        .expect("reaction cleanup recorder lock") = Some(Vec::new());
}

#[cfg(test)]
fn take_records() -> Vec<(u64, u64, char)> {
    REACTION_CLEANUP_RECORDS
        .lock()
        .expect("reaction cleanup recorder lock")
        .take()
        .unwrap_or_default()
}

#[cfg(test)]
async fn remove_reaction(
    _http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
) {
    if let Some(records) = REACTION_CLEANUP_RECORDS
        .lock()
        .expect("reaction cleanup recorder lock")
        .as_mut()
    {
        records.push((channel_id.get(), message_id.get(), emoji));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recovered_catch_up_message_removes_stale_hourglass() {
        let http = Arc::new(serenity::Http::new("Bot test-token"));
        let channel_id = ChannelId::new(1514499617272627231);
        let message_id = MessageId::new(1514500851761287319);

        begin_recording();
        cleanup_recovered_catch_up_hourglass(&http, channel_id, message_id).await;

        assert_eq!(
            take_records(),
            vec![(channel_id.get(), message_id.get(), '⏳')]
        );
    }
}
