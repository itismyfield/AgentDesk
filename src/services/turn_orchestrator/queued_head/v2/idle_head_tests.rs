// Windows has no directory fsync, so the queued V2 owner these tests adopt refuses every commit.
#![cfg(unix)]

use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, ChannelMailboxRegistry};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::sync::Arc;

fn item(id: u64) -> PendingQueueItem {
    serde_json::from_value(
        serde_json::json!({"author_id":7,"message_id":id,"channel_id":7301,
        "source_message_ids":[id],"queued_generation":5,"text":format!("input {id}")}),
    )
    .unwrap()
}

fn same(stored: &PendingQueueItem, offered: &PendingQueueItem) -> bool {
    stored.message_id == offered.message_id
}

async fn adopted(registry: &ChannelMailboxRegistry, channel: u64) -> ChannelMailboxHandle {
    let channel = ChannelId::new(channel);
    let path =
        super::super::super::pending_queue_file_path(&ProviderKind::Claude, "idle-head", channel)
            .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let handle = registry.handle(channel);
    handle.v2_adopt(path, "idle-head".into()).await.unwrap();
    handle
}

async fn admit(handle: &ChannelMailboxHandle, id: u64) -> (Admitted, bool) {
    let ticket = format!("idle-head-{id}");
    let (admitted, _, opened) = handle
        .v2_resume_ticket_at_head(item(id), ticket, same)
        .await
        .unwrap();
    (admitted, opened)
}

#[tokio::test]
async fn only_the_first_input_of_an_idle_owner_opens_its_head() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let handle = adopted(&registry, 7_301).await;

    assert_eq!(admit(&handle, 1).await, (Admitted::Queued, true));
    assert_eq!(
        admit(&handle, 2).await.1,
        false,
        "a second input waits behind the head"
    );
    assert_eq!(
        admit(&handle, 1).await.1,
        false,
        "a re-entry of the head opens nothing"
    );
}

#[tokio::test]
async fn a_running_turn_keeps_the_head_closed() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let handle = adopted(&registry, 7_302).await;
    let token = Arc::new(CancelToken::new());
    assert!(
        handle
            .try_start_turn(token, UserId::new(7), MessageId::new(99))
            .await
    );

    assert_eq!(admit(&handle, 3).await, (Admitted::Queued, false));
}
