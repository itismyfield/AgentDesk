use poise::serenity_prelude::{ChannelId, MessageId};

pub(super) fn forget_if_message(channel_id: ChannelId, message_id: Option<u64>) -> bool {
    message_id.map(MessageId::new).is_some_and(|message_id| {
        super::super::single_message_panel::completion_footer_forget_registered_target_if_message(
            channel_id, message_id,
        )
    })
}
