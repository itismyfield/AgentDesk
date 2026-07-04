use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::SharedData;

pub(in crate::services::discord) async fn cleanup_recovered_catch_up_hourglass(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
) {
    super::turn_view_reconciler::note_intake_turn_cleared(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        "recovered_catch_up_hourglass",
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recovered_catch_up_message_removes_stale_hourglass() {
        let http = Arc::new(serenity::Http::new("Bot test-token"));
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(1514499617272627231);
        let message_id = MessageId::new(1514500851761287319);

        cleanup_recovered_catch_up_hourglass(&http, &shared, channel_id, message_id).await;

        let ops = shared.turn_view_reconciler.ops();
        assert!(
            ops.iter().any(|op| {
                op.target.channel_id == channel_id
                    && op.target.message_id == message_id
                    && op.emoji == '⏳'
                    && !op.add
            }),
            "fresh cold-clear must issue a stale hourglass removal"
        );
    }
}
