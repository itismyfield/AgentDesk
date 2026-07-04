use poise::serenity_prelude::{ChannelId, MessageId};

pub(super) fn forget_if_message(channel_id: ChannelId, message_id: Option<u64>) -> bool {
    message_id.map(MessageId::new).is_some_and(|message_id| {
        super::super::footer_view_reconciler::note_footer_suppressed_for_message_takeover(
            channel_id, message_id,
        )
    })
}
