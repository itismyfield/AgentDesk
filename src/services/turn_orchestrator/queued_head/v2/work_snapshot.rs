use super::*;
use crate::services::turn_orchestrator::actor_lifetime::ActorEpoch;

#[derive(Debug, PartialEq, Eq)]
enum Blocker {
    Head,
    Closing,
    Effort,
    Retry,
}

struct WorkSnapshot {
    actor: ActorEpoch,
    items: usize,
    blocker: Option<Blocker>,
    due: Option<u64>,
}

impl ChannelMailboxHandle {
    async fn v2_work_snapshot(&self) -> Result<WorkSnapshot, String> {
        let actor = self.recovery_done().retirement.epoch();
        self.v2_request(move |slot| {
            let owner = slot.as_ref().ok_or("V2 not adopted")?;
            let items = owner.snapshot.items.len();
            let blocker = if owner.snapshot.head_injection.is_some() {
                Some(Blocker::Head)
            } else if items == 0 {
                None
            } else if owner.starts.closing.is_some() {
                Some(Blocker::Closing)
            } else {
                match owner.front() {
                    None => Some(Blocker::Effort),
                    Some(front) if owner.retry_waiting(&front) => Some(Blocker::Retry),
                    Some(_) => None,
                }
            };
            Ok(WorkSnapshot {
                actor,
                items,
                blocker,
                due: owner.settle_due(),
            })
        })
        .await
    }
}

#[tokio::test]
async fn drive_foundation_snapshot_distinguishes_empty_unadopted_and_blocked() {
    use crate::services::turn_orchestrator::ChannelMailboxRegistry;
    use poise::serenity_prelude::ChannelId;

    let root = tempfile::tempdir().unwrap();
    let registry = ChannelMailboxRegistry::default();
    let channel = ChannelId::new(7_272_846);
    let handle = registry.handle(channel);
    assert!(handle.v2_work_snapshot().await.is_err());
    handle
        .v2_adopt(root.path().join("queue.json"), "fixture".into())
        .await
        .unwrap();
    let empty = handle.v2_work_snapshot().await.unwrap();
    assert_eq!((empty.items, empty.blocker, empty.due), (0, None, None));

    handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            let item = serde_json::from_value(serde_json::json!({
                "message_id": 1, "channel_id": channel.get(), "author_id": 1,
                "text": "fixture", "queued_generation": 1, "source_message_ids": [1]
            }))
            .unwrap();
            assert_eq!(owner.enqueue_fixture(&item).ok(), Some(true));
            let front = owner.front().unwrap();
            assert!(owner.grant_front(&front)?);
            let grant = owner.grant_number_of(&front).unwrap();
            owner.enter_retry_wait_granted(
                (&front, grant),
                crate::services::turn_orchestrator::RetryWaitCause::RootRecoveryHeld,
                1_000,
            )
        })
        .await
        .unwrap()
        .expect("the exact grant entered its wait");
    let blocked = handle.v2_work_snapshot().await.unwrap();
    assert_eq!(blocked.actor, empty.actor);
    assert_eq!(
        (blocked.items, blocked.blocker, blocked.due),
        (1, Some(Blocker::Retry), Some(31_000))
    );
}
