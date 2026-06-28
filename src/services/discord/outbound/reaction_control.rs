use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::services::discord::SharedData;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum ReactionControlReplyReason {
    QueuedCardPostFailed,
    AlreadyStopping,
}

impl ReactionControlReplyReason {
    fn key(self) -> &'static str {
        match self {
            Self::QueuedCardPostFailed => "queued_card_post_failed",
            Self::AlreadyStopping => "already_stopping",
        }
    }
}

pub(in crate::services::discord) async fn send_reaction_control_reply(
    ctx: &serenity::Context,
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    message_id: serenity::MessageId,
    reason: ReactionControlReplyReason,
    content: &str,
) {
    let (correlation_id, semantic_event_id) =
        reaction_control_reply_delivery_ids(channel_id, message_id, reason);
    if let Err(error) = super::serenity_reference::send_referenced_lifecycle_notice(
        ctx.http.clone(),
        shared.clone(),
        channel_id,
        message_id,
        content,
        correlation_id,
        semantic_event_id,
    )
    .await
    {
        tracing::warn!(
            channel_id = channel_id.get(),
            message_id = message_id.get(),
            reason = reason.key(),
            error = %error,
            "[discord] reaction-control lifecycle notice delivery failed"
        )
    }
}

fn reaction_control_reply_delivery_ids(
    channel_id: serenity::ChannelId,
    message_id: serenity::MessageId,
    reason: ReactionControlReplyReason,
) -> (String, String) {
    (
        format!(
            "intake-reaction-control:{}:{}",
            channel_id.get(),
            message_id.get()
        ),
        format!(
            "intake-reaction-control:{}:{}:{}",
            channel_id.get(),
            message_id.get(),
            reason.key()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::{ReactionControlReplyReason, reaction_control_reply_delivery_ids};
    use poise::serenity_prelude::{ChannelId, MessageId};

    #[test]
    fn reaction_control_reply_ids_are_stable_per_message_and_reason() {
        let channel_id = ChannelId::new(123);
        let message_id = MessageId::new(456);

        let queued = reaction_control_reply_delivery_ids(
            channel_id,
            message_id,
            ReactionControlReplyReason::QueuedCardPostFailed,
        );
        let stopping = reaction_control_reply_delivery_ids(
            channel_id,
            message_id,
            ReactionControlReplyReason::AlreadyStopping,
        );

        assert_eq!(queued.0, "intake-reaction-control:123:456");
        assert_eq!(
            queued.1,
            "intake-reaction-control:123:456:queued_card_post_failed"
        );
        assert_eq!(stopping.0, "intake-reaction-control:123:456");
        assert_eq!(
            stopping.1,
            "intake-reaction-control:123:456:already_stopping"
        );
    }
}
