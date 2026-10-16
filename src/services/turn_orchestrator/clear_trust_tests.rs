//! After a clear, no actor of this process reads back a file the clear could not rewrite, and its
//! successors read back every write landed since; a restart may still read the old files.

use super::*;

fn other() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "queued-head-other", None)
}

/// The purge a live process runs: the idle actor goes and its successor takes the same fence.
async fn recreated(registry: &ChannelMailboxRegistry, channel: ChannelId) -> ChannelMailboxHandle {
    let removed = registry.remove_idle_entry(channel).await;
    assert_eq!(removed, registry_purge::MailboxPurgeOutcome::Removed);
    registry.handle(channel)
}

async fn hydrated(handle: &ChannelMailboxHandle, persistence: QueuePersistenceContext) -> Vec<u64> {
    let hydrated = handle.hydrate_pending_queue_from_disk(persistence).await;
    assert_eq!(hydrated.persistence_error, None);
    ids(queued(handle).await)
}

/// Fails every removal and publish of the channel's next clear, so its old files all stay.
fn fail_every_clear_write(channel: ChannelId) {
    ChannelMailboxRegistry::fail_clear_removals(channel, false, 2, false);
    ChannelMailboxRegistry::fail_clear_removals(channel, true, 2, false);
    fail_queue_saves(channel, 2);
}

/// A queue and an Unknown head a failed clear left on disk are neither hydrated nor a claim
/// barrier for the successor; only a restart reads them.
#[test]
fn a_successor_reads_nothing_a_failed_clear_left_on_disk() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_995_101);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let head = human(61, 7, "status?");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let ticket = reserve(&handle, &head).await;
        assert_eq!(
            settle(&handle, ticket, HeadOutcome::Unconfirmed).await,
            "unknown"
        );
        assert!(handle.enqueue(human(62, 8, "b"), ctx()).await.enqueued);
        let before = std::fs::read(queue_path(channel)).unwrap();
        fail_every_clear_write(channel);

        let cleared = handle.clear(ctx()).await;

        let failed = ["queue_unlink", "marker_unlink", "queue_publish"];
        assert_eq!(cleared.persistence.failed, failed);
        let successor = recreated(&registry, channel).await;
        assert!(hydrated(&successor, ctx()).await.is_empty());
        assert_eq!(std::fs::read(queue_path(channel)).unwrap(), before);
        let order = TurnAdmissionOrder::BehindQueue;
        assert!(
            starts(&successor, 64, order).await,
            "the head left on disk holds no claim"
        );
        assert_eq!(
            restarted(channel).await,
            [61, 62],
            "a restart reads what stayed"
        );
    });
}

/// A dispatch marker a failed clear left is never hydrated, taken, merged or a claim's owner.
#[test]
fn a_successor_never_takes_a_marker_a_failed_clear_left() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_995_102);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let dequeued = human(71, 7, "dequeued");
        assert!(handle.enqueue(dequeued.clone(), ctx()).await.enqueued);
        assert_eq!(taken(&handle).await, Some(71));
        ChannelMailboxRegistry::fail_clear_removals(channel, true, 2, false);

        let cleared = handle.clear(ctx()).await;

        assert_eq!(cleared.persistence.failed, ["marker_unlink"]);
        assert!(queue_path(channel).with_extension("dispatch").exists());
        let successor = recreated(&registry, channel).await;
        assert!(hydrated(&successor, ctx()).await.is_empty());
        assert_eq!(taken(&successor).await, None);
        let merged = successor
            .merge_restored_dispatch_marker(dequeued, None, ctx())
            .await;
        assert_eq!(merged.absorbed, 0);
        let next = human(73, 8, "next");
        assert!(successor.enqueue(next.clone(), ctx()).await.enqueued);
        let _ticket = reserve(&successor, &next).await;
    });
}

/// Claims `head` in `persistence`, marks it possibly sent and records it submitted.
async fn submit_in(
    handle: &ChannelMailboxHandle,
    head: &Intervention,
    persistence: QueuePersistenceContext,
) {
    let claim = handle.claim_queued_head(QueueIdentity::of(head), persistence.clone());
    let QueuedHeadClaim::Reserved(ticket) = claim.await else {
        panic!("not reserved")
    };
    let sent = handle.settle_queued_head(ticket, HeadOutcome::MayHaveSent, persistence.clone());
    let HeadSettle::Committed(Some(ticket)) = sent.await else {
        panic!("not committed")
    };
    let injected = handle.settle_queued_head(ticket, HeadOutcome::Injected, persistence);
    assert!(matches!(injected.await, HeadSettle::Committed(_)));
}

/// Whether `item` is refused as already submitted.
async fn refused_as_submitted(
    handle: &ChannelMailboxHandle,
    item: Intervention,
    persistence: QueuePersistenceContext,
) -> bool {
    let again = handle.enqueue(item, persistence).await;
    again.refusal_reason == Some(EnqueueRefusalReason::AlreadyActiveTurn)
}

/// The submitted record the actor writes after a failed clear is read back by its successor; a
/// second failed clear distrusts it again.
#[test]
fn a_successor_reads_back_a_write_landed_after_the_clear_until_the_next_clear() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_995_103);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        assert!(handle.enqueue(human(81, 7, "before"), ctx()).await.enqueued);
        fail_every_clear_write(channel);
        let _ = handle.clear(ctx()).await;
        let sent = human(82, 8, "after");
        assert!(handle.enqueue(sent.clone(), ctx()).await.enqueued);
        submit_in(&handle, &sent, ctx()).await;

        let successor = recreated(&registry, channel).await;
        assert!(hydrated(&successor, ctx()).await.is_empty());
        assert!(refused_as_submitted(&successor, sent.clone(), ctx()).await);

        fail_every_clear_write(channel);
        let _ = successor.clear(ctx()).await;
        let next = recreated(&registry, channel).await;
        assert!(hydrated(&next, ctx()).await.is_empty());
        assert!(
            !refused_as_submitted(&next, sent, ctx()).await,
            "the record is not read"
        );
    });
}

/// A write that lands in one namespace never vouches for a file a clear left in another.
#[test]
fn a_write_in_one_namespace_never_trusts_what_a_clear_left_in_another() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_995_104);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        assert!(handle.enqueue(human(91, 7, "stale"), ctx()).await.enqueued);
        fail_every_clear_write(channel);
        let _ = handle.clear(ctx()).await;
        let fresh = human(92, 8, "fresh");
        assert!(handle.enqueue(fresh.clone(), other()).await.enqueued);
        submit_in(&handle, &fresh, other()).await;

        let successor = recreated(&registry, channel).await;
        assert!(hydrated(&successor, ctx()).await.is_empty());
        assert!(refused_as_submitted(&successor, fresh, other()).await);
    });
}
