use std::sync::Arc;

use poise::serenity_prelude::MessageId;

use super::{ChannelMailboxHandle, ChannelMailboxMsg, DispatchLease, QueuePersistenceContext};

impl ChannelMailboxHandle {
    pub(crate) async fn abandon_pending_dispatch(
        &self,
        user_message_id: MessageId,
        persistence: QueuePersistenceContext,
    ) {
        let _ = self
            .request(
                |reply| ChannelMailboxMsg::AbandonPendingDispatch {
                    user_message_id,
                    dispatch_lease: None,
                    persistence,
                    consume_marker: true,
                    reply,
                },
                false,
            )
            .await;
    }

    pub(crate) async fn abandon_pending_dispatch_if_lease_matches(
        &self,
        user_message_id: MessageId,
        dispatch_lease: Arc<DispatchLease>,
        persistence: QueuePersistenceContext,
    ) -> bool {
        self.request(
            |reply| ChannelMailboxMsg::AbandonPendingDispatch {
                user_message_id,
                dispatch_lease: Some(dispatch_lease),
                persistence,
                consume_marker: true,
                reply,
            },
            false,
        )
        .await
    }

    pub(crate) async fn clear_pending_dispatch_reservation(
        &self,
        user_message_id: MessageId,
        persistence: QueuePersistenceContext,
    ) {
        let _ = self
            .request(
                |reply| ChannelMailboxMsg::AbandonPendingDispatch {
                    user_message_id,
                    dispatch_lease: None,
                    persistence,
                    consume_marker: false,
                    reply,
                },
                false,
            )
            .await;
    }
}
