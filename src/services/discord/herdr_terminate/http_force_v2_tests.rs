//! A forced Herdr cancel on a channel whose queue the V2 input owner holds.
use super::*;
use crate::services::turn_orchestrator::{PendingQueueItem, v2_queue};

/// The rig for test `name` in its own child, on a current-thread runtime so every probe runs.
fn rig_v2(name: &str) -> Option<(herdr::P11TerminationRig, tokio::runtime::Runtime)> {
    if !herdr::in_child(&format!("{}::{name}", module_path!()), CHANNEL) {
        return None;
    }
    let rig = herdr::P11TerminationRig::boot_with(CHANNEL, false);
    rig.thread_installer()();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(rig.thread_installer())
        .build()
        .unwrap();
    Some((rig, runtime))
}

/// The cancel captures the owner's queue and purges it as captured, so the input it drops never
/// starts afterwards and the reply counts it.
#[test]
fn a_forced_herdr_cancel_purges_the_owner_queue_pg() {
    let Some((rig, runtime)) = rig_v2("a_forced_herdr_cancel_purges_the_owner_queue_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let handle = node.shared.mailbox(channel());
        let persistence = crate::services::discord::queue_persistence_context(
            &node.shared,
            &ProviderKind::Claude,
            channel(),
        );
        let (path, namespace) = v2_queue(&persistence, channel()).unwrap();
        handle.v2_adopt(path, namespace).await.unwrap();
        let queued: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
            "message_id":62,"channel_id":CHANNEL,"source_message_ids":[62],
            "queued_generation":1,"text":"queued behind the cancelled turn"}))
        .unwrap();
        let enqueued = handle.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            owner
                .enqueue_fixture(&queued)
                .map_err(|error| format!("{error:?}"))
        });
        assert_eq!(enqueued.await, Ok(true));

        let (status, reply) = node.cancel().await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(reply["queue_purged"], 1, "{reply}");
        let front = handle.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.front()));
        assert!(
            front.await.unwrap().is_none(),
            "a purged input never starts"
        );
        node.finish().await;
    });
}
