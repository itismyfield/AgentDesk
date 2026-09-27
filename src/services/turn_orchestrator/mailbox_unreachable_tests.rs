use std::sync::Arc;

use tokio::sync::mpsc;

use super::actor_hydrate_regression_tests::make_intervention;
use super::{
    ChannelMailboxHandle, ChannelMailboxRegistry, EnqueueRefusalReason, GLOBAL_CHANNEL_MAILBOXES,
    MailboxUnreachable, QueuePersistenceContext, RecoveryDoneSignal, spawn_channel_mailbox,
};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;

pub(crate) fn closed_handle() -> ChannelMailboxHandle {
    let (sender, receiver) = mpsc::unbounded_channel();
    drop(receiver);
    ChannelMailboxHandle {
        sender,
        recovery_done: Arc::new(RecoveryDoneSignal::new()),
    }
}

impl ChannelMailboxRegistry {
    pub(crate) fn insert_unreachable_for_test(&self, channel_id: ChannelId) {
        let handle = closed_handle();
        self.handles.insert(channel_id, handle.clone());
        GLOBAL_CHANNEL_MAILBOXES.insert(channel_id, handle);
    }
}

fn reply_dropping_handle() -> ChannelMailboxHandle {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = receiver.recv().await {
            drop(msg);
        }
    });
    ChannelMailboxHandle {
        sender,
        recovery_done: Arc::new(RecoveryDoneSignal::new()),
    }
}

async fn assert_turn_queries_unreachable(handle: &ChannelMailboxHandle) {
    assert_eq!(handle.has_active_turn().await, Err(MailboxUnreachable));
    assert_eq!(
        handle.has_blocking_active_turn().await,
        Err(MailboxUnreachable)
    );
    assert!(matches!(
        handle.cancel_token().await,
        Err(MailboxUnreachable)
    ));
}

#[tokio::test]
async fn measured_idle_is_distinct_from_unreachable_actor() {
    let live = spawn_channel_mailbox(
        ChannelId::new(6046),
        Default::default(),
        Arc::new(RecoveryDoneSignal::new()),
    );
    assert_eq!(live.has_active_turn().await, Ok(false));
    assert_eq!(live.has_blocking_active_turn().await, Ok(false));
    assert!(matches!(live.cancel_token().await, Ok(None)));

    assert_turn_queries_unreachable(&closed_handle()).await;
    assert_turn_queries_unreachable(&reply_dropping_handle()).await;
}

#[tokio::test]
async fn requeue_front_to_unreachable_actor_names_the_refusal() {
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, "unreachable", None);
    let intervention = make_intervention(6_046_002, "head", std::time::Instant::now());

    let result = closed_handle()
        .requeue_front(intervention, persistence)
        .await;

    assert!(!result.enqueued);
    assert_eq!(
        result.refusal_reason,
        Some(EnqueueRefusalReason::ActorUnreachable)
    );
}
