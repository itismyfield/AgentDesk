use tokio::sync::mpsc;

use super::{ChannelMailboxHandle, MailboxUnreachable, spawn_channel_mailbox};
use poise::serenity_prelude::ChannelId;

fn closed_handle() -> ChannelMailboxHandle {
    let (sender, receiver) = mpsc::unbounded_channel();
    drop(receiver);
    ChannelMailboxHandle { sender }
}

fn reply_dropping_handle() -> ChannelMailboxHandle {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = receiver.recv().await {
            drop(msg);
        }
    });
    ChannelMailboxHandle { sender }
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
    let live = spawn_channel_mailbox(ChannelId::new(6046));
    assert_eq!(live.has_active_turn().await, Ok(false));
    assert_eq!(live.has_blocking_active_turn().await, Ok(false));
    assert!(matches!(live.cancel_token().await, Ok(None)));

    assert_turn_queries_unreachable(&closed_handle()).await;
    assert_turn_queries_unreachable(&reply_dropping_handle()).await;
}
