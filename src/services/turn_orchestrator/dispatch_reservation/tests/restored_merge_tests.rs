//! A boot snapshot merged after its items left the queue file never brings them back,
//! while items the file still holds are restored as before.

use super::*;
use crate::services::turn_orchestrator::pending_queue_persistence::pending_queue_file_path;
use crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome;
use crate::services::turn_orchestrator::{ChannelMailboxHandle, SourceMessageTextSegment};
use crate::services::turn_orchestrator::{
    MAX_INTERVENTIONS_PER_CHANNEL, load_pending_queues,
    remove_channel_pending_queue_files_all_tokens, save_channel_queue,
};

const TOKEN: &str = "restored-merge";

fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN, None)
}

fn item(id: u64) -> Intervention {
    single_source_intervention(MessageId::new(id), &format!("input {id}"))
}

fn ids(queue: &[Intervention]) -> Vec<u64> {
    queue.iter().map(|item| item.message_id.get()).collect()
}

fn disk_ids(channel: ChannelId) -> Vec<u64> {
    ids(&load_channel_pending_queue(&ProviderKind::Claude, TOKEN, channel).0)
}

/// Leaves `items` in the queue file as a previous process did and reads them back with the boot
/// loader, as the boot restore does before its awaits.
fn boot_snapshot(channel: ChannelId, items: &[Intervention]) -> Vec<Intervention> {
    save_channel_queue(&ProviderKind::Claude, TOKEN, channel, items, None).unwrap();
    let mut queues = load_pending_queues(&ProviderKind::Claude, TOKEN).0;
    queues.remove(&channel).unwrap_or_default()
}

fn with_root<F: std::future::Future<Output = ()>>(test: impl FnOnce() -> F) {
    let _lock = lock_test_env();
    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set_root(tmp.path());
    run_async(test());
}

async fn memory(handle: &ChannelMailboxHandle) -> Vec<u64> {
    ids(&handle.snapshot().await.intervention_queue)
}

#[test]
fn a_boot_item_cleared_before_the_merge_stays_cleared() {
    with_root(|| async {
        let channel = ChannelId::new(6_258_001);
        let snapshot = boot_snapshot(channel, &[item(6_258_011)]);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        handle.clear(ctx()).await;
        assert!(disk_ids(channel).is_empty(), "the clear emptied the file");

        let merged = handle.merge_restored_queue_items(snapshot, ctx()).await;
        assert_eq!((merged.absorbed, merged.persistence_error), (0, None));
        assert!(memory(&handle).await.is_empty(), "Q stays discarded");
        assert!(disk_ids(channel).is_empty(), "Q is not written back");
    });
}

#[test]
fn a_boot_item_cleared_before_the_merge_leaves_a_later_enqueue_alone() {
    with_root(|| async {
        let channel = ChannelId::new(6_258_002);
        let snapshot = boot_snapshot(channel, &[item(6_258_021)]);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        handle.enqueue(item(6_258_022), ctx()).await;
        handle.clear(ctx()).await;
        assert!(handle.enqueue(item(6_258_023), ctx()).await.enqueued);

        handle.merge_restored_queue_items(snapshot, ctx()).await;
        assert_eq!(memory(&handle).await, [6_258_023]);
        assert_eq!(disk_ids(channel), [6_258_023]);
    });
}

#[test]
fn a_boot_item_purged_before_the_merge_stays_purged() {
    with_root(|| async {
        let channel = ChannelId::new(6_258_003);
        let snapshot = boot_snapshot(channel, &[item(6_258_031)]);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(item(6_258_032), ctx()).await.enqueued);
        assert_eq!(memory(&handle).await, [6_258_031, 6_258_032]);
        assert_eq!(handle.purge_queue(ctx(), false).await.drained, 2);

        let merged = handle.merge_restored_queue_items(snapshot, ctx()).await;
        assert_eq!(merged.absorbed, 0);
        assert!(memory(&handle).await.is_empty());
        assert!(disk_ids(channel).is_empty());
    });
}

#[test]
fn a_boot_item_whose_file_an_actorless_force_purge_removed_stays_removed() {
    with_root(|| async {
        let channel = ChannelId::new(6_258_004);
        let snapshot = boot_snapshot(channel, &[item(6_258_041)]);
        assert_eq!(
            remove_channel_pending_queue_files_all_tokens(&ProviderKind::Claude, channel),
            1
        );

        let handle = ChannelMailboxRegistry::default().handle(channel);
        let merged = handle.merge_restored_queue_items(snapshot, ctx()).await;
        assert_eq!(merged.absorbed, 0);
        assert!(memory(&handle).await.is_empty());
        let path = pending_queue_file_path(&ProviderKind::Claude, TOKEN, channel).unwrap();
        assert!(!path.exists(), "the purged file is not recreated");
    });
}

#[test]
fn a_successor_does_not_take_a_boot_item_its_predecessor_cleared() {
    with_root(|| async {
        let channel = ChannelId::new(6_258_005);
        let snapshot = boot_snapshot(channel, &[item(6_258_051)]);
        let registry = ChannelMailboxRegistry::default();
        let predecessor = registry.handle(channel);
        assert!(predecessor.enqueue(item(6_258_052), ctx()).await.enqueued);
        predecessor.clear(ctx()).await;
        let purge = registry.remove_idle_entry(channel).await;
        assert!(matches!(purge, MailboxPurgeOutcome::Removed), "{purge:?}");

        let successor = registry.handle(channel);
        let merged = successor.merge_restored_queue_items(snapshot, ctx()).await;
        assert_eq!(merged.absorbed, 0);
        assert!(memory(&successor).await.is_empty());
        assert!(disk_ids(channel).is_empty());
    });
}

#[test]
fn a_successor_restores_a_boot_item_nothing_discarded() {
    with_root(|| async {
        let channel = ChannelId::new(6_258_006);
        let snapshot = boot_snapshot(channel, &[item(6_258_061)]);
        let registry = ChannelMailboxRegistry::default();
        registry.handle(channel);
        let purge = registry.remove_idle_entry(channel).await;
        assert!(matches!(purge, MailboxPurgeOutcome::Removed), "{purge:?}");

        let successor = registry.handle(channel);
        let merged = successor.merge_restored_queue_items(snapshot, ctx()).await;
        assert_eq!(merged.absorbed, 1);
        assert_eq!(memory(&successor).await, [6_258_061]);
        assert_eq!(disk_ids(channel), [6_258_061]);
    });
}

#[test]
fn a_boot_item_dequeued_before_the_merge_is_not_queued_again() {
    with_root(|| async {
        let channel = ChannelId::new(6_258_007);
        let snapshot = boot_snapshot(channel, &[item(6_258_071)]);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(item(6_258_072), ctx()).await.enqueued);
        let taken = handle.take_next_soft(ctx()).await.intervention;
        assert_eq!(taken.map(|item| item.message_id.get()), Some(6_258_071));

        let merged = handle.merge_restored_queue_items(snapshot, ctx()).await;
        assert_eq!(merged.absorbed, 0);
        assert_eq!(
            memory(&handle).await,
            [6_258_072],
            "Q runs once, from its marker"
        );
        assert_eq!(disk_ids(channel), [6_258_072]);
    });
}

#[test]
fn an_undisturbed_boot_snapshot_is_restored_in_order() {
    with_root(|| async {
        let channel = ChannelId::new(6_258_008);
        let snapshot = boot_snapshot(channel, &[item(6_258_081), item(6_258_082)]);
        let handle = ChannelMailboxRegistry::default().handle(channel);

        let merged = handle.merge_restored_queue_items(snapshot, ctx()).await;
        assert_eq!((merged.absorbed, merged.persistence_error), (2, None));
        assert_eq!(memory(&handle).await, [6_258_081, 6_258_082]);
        assert_eq!(disk_ids(channel), [6_258_081, 6_258_082]);
    });
}

#[test]
fn a_live_enqueue_before_the_merge_keeps_both_items() {
    with_root(|| async {
        let channel = ChannelId::new(6_258_009);
        let snapshot = boot_snapshot(channel, &[item(6_258_091)]);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(item(6_258_092), ctx()).await.enqueued);

        let merged = handle.merge_restored_queue_items(snapshot, ctx()).await;
        assert_eq!(merged.persistence_error, None);
        assert_eq!(memory(&handle).await, [6_258_091, 6_258_092]);
        assert_eq!(disk_ids(channel), [6_258_091, 6_258_092]);
    });
}

#[test]
fn only_the_sources_the_file_still_holds_are_restored() {
    with_root(|| async {
        let channel = ChannelId::new(6_258_010);
        let (a, b) = (MessageId::new(6_258_101), MessageId::new(6_258_102));
        let mut merged_item = make_intervention(b, &[a, b], "a\nb");
        merged_item.source_text_segments = vec![
            SourceMessageTextSegment::new(a, "a"),
            SourceMessageTextSegment::new(b, "b"),
        ];
        let snapshot = boot_snapshot(channel, &[merged_item]);
        // Another writer consumed source A after the boot read.
        save_channel_queue(
            &ProviderKind::Claude,
            TOKEN,
            channel,
            &[item(b.get())],
            None,
        )
        .unwrap();

        let handle = ChannelMailboxRegistry::default().handle(channel);
        let merged = handle.merge_restored_queue_items(snapshot, ctx()).await;
        assert_eq!(merged.absorbed, 1);
        let queue = handle.snapshot().await.intervention_queue;
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].source_message_ids, [b], "source A is not restored");
        assert_eq!(queue[0].text, "b");
        assert_eq!(disk_ids(channel), [b.get()]);
    });
}

#[test]
fn an_overflow_eviction_before_the_merge_stays_evicted() {
    with_root(|| async {
        let channel = ChannelId::new(6_258_012);
        let first = 6_258_200;
        let items: Vec<Intervention> = (0..=MAX_INTERVENTIONS_PER_CHANNEL as u64)
            .map(|n| item(first + n))
            .collect();
        let snapshot = boot_snapshot(channel, &items);
        assert_eq!(snapshot.len(), MAX_INTERVENTIONS_PER_CHANNEL + 1);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        handle.hydrate_pending_queue_from_disk(ctx()).await;
        let probe = handle.has_pending_soft_queue(ctx()).await;
        assert_eq!(
            probe.queue_exit_events.len(),
            1,
            "the oldest item overflowed"
        );
        assert!(!disk_ids(channel).contains(&first));

        handle.merge_restored_queue_items(snapshot, ctx()).await;
        let queue = memory(&handle).await;
        assert_eq!(queue.len(), MAX_INTERVENTIONS_PER_CHANNEL);
        assert!(
            !queue.contains(&first),
            "the evicted item is not merged back"
        );
        assert!(!disk_ids(channel).contains(&first));
    });
}

#[test]
fn a_queue_file_the_merge_cannot_read_merges_nothing_and_reports_its_error() {
    with_root(|| async {
        let channel = ChannelId::new(6_258_013);
        let snapshot = boot_snapshot(channel, &[item(6_258_131)]);
        let path = pending_queue_file_path(&ProviderKind::Claude, TOKEN, channel).unwrap();
        let torn = br#"[{"message_id": 6258131, "te"#;
        std::fs::write(&path, torn).unwrap();
        let handle = ChannelMailboxRegistry::default().handle(channel);

        let merged = handle.merge_restored_queue_items(snapshot, ctx()).await;
        assert_eq!(merged.absorbed, 0);
        assert!(merged.persistence_error.is_some(), "{merged:?}");
        assert!(memory(&handle).await.is_empty(), "Q is not merged");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            torn,
            "the file is left as it was"
        );
    });
}
