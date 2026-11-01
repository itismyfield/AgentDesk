//! A forced cancel on a channel whose queue the V2 owner holds.
use super::*;
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem, v2_queue};

fn item(id: u64, channel: u64) -> PendingQueueItem {
    serde_json::from_value(
        serde_json::json!({"author_id":7,"message_id":id,"channel_id":channel,
        "source_message_ids":[id],"queued_generation":1,"text":format!("queued {id}")}),
    )
    .unwrap()
}

/// The forced purge removes what the owner queued, so nothing the cancel meant to drop runs
/// after it: the owner has no front left to start.
#[tokio::test(flavor = "current_thread")]
async fn a_forced_cancel_purges_the_v2_queue_and_nothing_reruns() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (provider, channel) = (ProviderKind::Claude, ChannelId::new(7_272_562));
    let handle = ChannelMailboxRegistry::default().handle(channel);
    let persistence = QueuePersistenceContext::new(&provider, "", None);
    let (path, namespace) = v2_queue(&persistence, channel).unwrap();
    handle.v2_adopt(path, namespace).await.unwrap();
    for id in [8_000_000_000_000_562_001, 8_000_000_000_000_562_002] {
        let queued = item(id, channel.get());
        let enqueued = handle.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            owner
                .enqueue_fixture(&queued)
                .map_err(|error| format!("{error:?}"))
        });
        assert_eq!(enqueued.await, Ok(true));
    }
    let target = TurnLifecycleTarget {
        provider: Some(provider),
        channel_id: Some(channel),
        tmux_name: String::new(),
    };
    let purged = force_purge_channel_mailbox(None, &target, None).await;
    assert_eq!(purged.map(|counts| counts.drained), Some(2));
    let front = handle.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.front()));
    assert!(
        front.await.unwrap().is_none(),
        "a purged input never starts"
    );
}
