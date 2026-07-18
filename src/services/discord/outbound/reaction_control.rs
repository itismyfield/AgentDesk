use std::sync::Arc;

use poise::serenity_prelude as serenity;

use crate::services::discord::SharedData;

#[cfg(test)]
thread_local! {
    static TEST_REPLY_DELIVERIES: std::cell::RefCell<Vec<ReactionControlReplyReason>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum ReactionControlReplyReason {
    QueuedCardPostFailed,
    QueueReactionFailed,
}

impl ReactionControlReplyReason {
    fn key(self) -> &'static str {
        match self {
            Self::QueuedCardPostFailed => "queued_card_post_failed",
            Self::QueueReactionFailed => "queue_reaction_failed",
        }
    }
}

pub(in crate::services::discord) async fn ensure_queue_reaction_or_fallback_http(
    http: &Arc<serenity::http::Http>,
    channel_id: serenity::ChannelId,
    shared: &Arc<SharedData>,
    message_id: serenity::MessageId,
    delivered: bool,
) -> bool {
    if !delivered {
        send_reaction_control_reply_http(
            http,
            channel_id,
            shared,
            message_id,
            ReactionControlReplyReason::QueueReactionFailed,
            "📬 큐에 추가됨 — 리액션 표시는 실패했지만 메시지는 큐잉되었습니다.",
        )
        .await;
    }
    delivered
}

pub(in crate::services::discord) async fn send_reaction_control_reply(
    ctx: &serenity::Context,
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    message_id: serenity::MessageId,
    reason: ReactionControlReplyReason,
    content: &str,
) {
    send_reaction_control_reply_http(&ctx.http, channel_id, shared, message_id, reason, content)
        .await;
}

pub(in crate::services::discord) async fn send_reaction_control_reply_http(
    http: &Arc<serenity::http::Http>,
    channel_id: serenity::ChannelId,
    shared: &Arc<SharedData>,
    message_id: serenity::MessageId,
    reason: ReactionControlReplyReason,
    content: &str,
) {
    #[cfg(test)]
    {
        let _ = (http, channel_id, shared, message_id, content);
        TEST_REPLY_DELIVERIES.with(|deliveries| deliveries.borrow_mut().push(reason));
        return;
    }
    #[cfg(not(test))]
    let (correlation_id, semantic_event_id) =
        reaction_control_reply_delivery_ids(channel_id, message_id, reason);
    #[cfg(not(test))]
    if let Err(error) = super::serenity_reference::send_referenced_lifecycle_notice(
        http.clone(),
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

#[cfg(test)]
pub(in crate::services::discord) fn take_test_reply_deliveries() -> Vec<ReactionControlReplyReason>
{
    TEST_REPLY_DELIVERIES.with(|deliveries| std::mem::take(&mut *deliveries.borrow_mut()))
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
    use std::sync::Arc;

    use super::{
        ReactionControlReplyReason, ensure_queue_reaction_or_fallback_http,
        reaction_control_reply_delivery_ids, take_test_reply_deliveries,
    };
    use poise::serenity_prelude::{ChannelId, MessageId};

    #[test]
    fn reaction_control_reply_ids_are_stable_for_queued_card_failure() {
        let channel_id = ChannelId::new(123);
        let message_id = MessageId::new(456);

        let queued = reaction_control_reply_delivery_ids(
            channel_id,
            message_id,
            ReactionControlReplyReason::QueuedCardPostFailed,
        );
        assert_eq!(queued.0, "intake-reaction-control:123:456");
        assert_eq!(
            queued.1,
            "intake-reaction-control:123:456:queued_card_post_failed"
        );
        let reaction = reaction_control_reply_delivery_ids(
            channel_id,
            message_id,
            ReactionControlReplyReason::QueueReactionFailed,
        );
        assert_eq!(reaction.0, queued.0);
        assert_eq!(
            reaction.1,
            "intake-reaction-control:123:456:queue_reaction_failed"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queue_reaction_delivery_uses_fallback_only_on_failure() {
        assert!(take_test_reply_deliveries().is_empty());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(poise::serenity_prelude::Http::new("Bot test-token"));
        let channel_id = ChannelId::new(455_400_000_000_301);
        let message_id = MessageId::new(455_400_000_000_302);

        assert!(
            ensure_queue_reaction_or_fallback_http(&http, channel_id, &shared, message_id, true,)
                .await
        );
        assert!(
            take_test_reply_deliveries().is_empty(),
            "successful queue reactions must remain reaction-only"
        );

        assert!(
            !ensure_queue_reaction_or_fallback_http(&http, channel_id, &shared, message_id, false,)
                .await
        );
        assert_eq!(
            take_test_reply_deliveries(),
            vec![ReactionControlReplyReason::QueueReactionFailed],
            "a failed queue reaction must emit exactly one referenced fallback"
        );
    }
}
