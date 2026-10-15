//! A hydrate that meets an unreadable queue file while a dispatch marker is on disk.

use super::*;
use crate::services::turn_orchestrator::ChannelMailboxHandle;
use crate::services::turn_orchestrator::pending_queue_persistence::pending_queue_file_path;

const TOKEN: &str = "hydrate-corrupt-queue";

fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN, None)
}

fn queue_file(channel: ChannelId) -> std::path::PathBuf {
    pending_queue_file_path(&ProviderKind::Claude, TOKEN, channel).unwrap()
}

fn plant_marker(channel: ChannelId, head: &Intervention) {
    save_channel_pending_dispatch_marker(&ProviderKind::Claude, TOKEN, channel, head, None)
        .unwrap();
}

/// The queue file, the marker file and the queued message ids. Bytes are escaped without loss, so
/// equal text means equal bytes.
type Kept = (Option<String>, Option<String>, Vec<u64>);

async fn kept(handle: &ChannelMailboxHandle, channel: ChannelId) -> Kept {
    let read = |path: std::path::PathBuf| {
        let bytes = std::fs::read(path).ok();
        bytes.map(|bytes| bytes.escape_ascii().to_string())
    };
    let queue = handle.snapshot().await.intervention_queue;
    (
        read(queue_file(channel)),
        read(queue_file(channel).with_extension("dispatch")),
        queue.iter().map(|item| item.message_id.get()).collect(),
    )
}

/// A hydrate and then an enqueue of `next` each report a persistence error, and neither changes
/// the damaged file, the marker or the `queued` ids.
async fn refuses_hydrate_then_enqueue(
    handle: &ChannelMailboxHandle,
    channel: ChannelId,
    damaged: &[u8],
    queued: &[u64],
    next: u64,
) {
    let before = kept(handle, channel).await;
    let damaged = Some(damaged.escape_ascii().to_string());
    assert!(before.1.is_some(), "the marker fixture is on disk");
    assert_eq!((&before.0, &before.2[..]), (&damaged, queued));

    let hydrate = handle
        .hydrate_pending_queue_from_disk_or_refused(ctx())
        .await
        .expect("the actor answers the hydrate");
    assert_eq!(
        (
            hydrate.persistence_error.is_some(),
            hydrate.absorbed,
            kept(handle, channel).await
        ),
        (true, 0, before.clone()),
        "hydrate: (reported an error, absorbed, (queue file, marker file, queued ids))"
    );

    let next = single_source_intervention(MessageId::new(next), "next request");
    let enqueue = handle.enqueue(next, ctx()).await;
    assert_eq!(
        (
            enqueue.persistence_error.is_some(),
            enqueue.enqueued,
            kept(handle, channel).await
        ),
        (true, false, before),
        "enqueue: (reported an error, enqueued, (queue file, marker file, queued ids))"
    );
}

/// With `damaged` as the queue file, the marker is the only readable copy of its message. After a
/// restart nothing absorbs it; on a live actor that still queues the message nothing drops it.
fn a_damaged_queue_file_keeps_its_dispatch_marker(damaged: &[u8], base: u64) {
    let _lock = lock_test_env();
    let tmp = tempfile::tempdir().unwrap();
    let _env_guard = EnvGuard::set_root(tmp.path());
    run_async(async {
        let head = single_source_intervention(MessageId::new(base + 1), "dispatched head");

        let restarted = ChannelId::new(base);
        std::fs::create_dir_all(queue_file(restarted).parent().unwrap()).unwrap();
        std::fs::write(queue_file(restarted), damaged).unwrap();
        plant_marker(restarted, &head);
        let handle = ChannelMailboxRegistry::default().handle(restarted);
        refuses_hydrate_then_enqueue(&handle, restarted, damaged, &[], base + 2).await;

        let live = ChannelId::new(base + 10);
        let handle = ChannelMailboxRegistry::default().handle(live);
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        plant_marker(live, &head);
        std::fs::write(queue_file(live), damaged).unwrap();
        refuses_hydrate_then_enqueue(&handle, live, damaged, &[base + 1], base + 2).await;
    });
}

#[test]
fn a_hydrate_over_a_torn_queue_file_keeps_the_file_and_its_dispatch_marker() {
    let torn = br#"{"version":1,"items":[{"author_id":7,"message_id":71,"#;
    a_damaged_queue_file_keeps_its_dispatch_marker(torn, 62_590_100);
}

#[test]
fn a_hydrate_over_a_non_utf8_queue_file_keeps_the_file_and_its_dispatch_marker() {
    let non_utf8 = b"[{\"author_id\":7,\"message_id\":71,\"text\":\"\xff\"}]";
    a_damaged_queue_file_keeps_its_dispatch_marker(non_utf8, 62_590_200);
}
