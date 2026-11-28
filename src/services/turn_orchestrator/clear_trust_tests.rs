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

mod actorless_purge {
    use std::sync::{Arc, atomic::Ordering};

    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    use crate::services::discord::health::HealthRegistry;
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::queue::force_purge_channel_mailbox;
    use crate::services::turn_lifecycle::TurnLifecycleTarget;
    use crate::services::turn_orchestrator::ChannelMailboxRegistry;
    use crate::services::turn_orchestrator::queued_head::{
        QueueIdentity,
        root_test_support::{TOKEN, bytes, ctx, human, path, queue, reserve_unknown, root, run},
    };

    /// Without a live actor, force purge removes only namespaces whose file holds no head or
    /// submitted source and parses cleanly.
    #[test]
    fn an_actorless_force_purge_keeps_held_or_unreadable_snapshots() {
        let _root = root();
        run(async {
            let channel = ChannelId::new(6_813_171);
            let item = r#"{"author_id":7,"message_id":71,"text":"h"}"#;
            let identity = r#"{"primary":71,"queued_generation":5,"sources":[{"message_id":71,"queued_generation":5}]}"#;
            let unknown = format!(
                r#"{{"version":1,"items":[{item}],"head_injection":{{"identity":{identity},"original":{item},"phase":"unknown"}}}}"#
            );
            let terminal =
                format!(r#"{{"version":1,"items":[],"terminal_identities":[{identity}]}}"#);
            let base =
                crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
            let file = |token: &str| {
                base.join("claude")
                    .join(token)
                    .join(format!("{}.json", channel.get()))
            };
            for (token, body) in [
                ("unknown", unknown.as_str()),
                ("terminal", terminal.as_str()),
                ("legacy", "[]"),
                ("torn", r#"{"version":1,"items":["#),
                ("broken", "[{"),
            ] {
                std::fs::create_dir_all(file(token).parent().unwrap()).unwrap();
                std::fs::write(file(token), body).unwrap();
            }
            let target = TurnLifecycleTarget {
                provider: Some(ProviderKind::Claude),
                channel_id: Some(channel),
                tmux_name: String::new(),
            };
            let purged =
                force_purge_channel_mailbox(None::<&Arc<HealthRegistry>>, &target, None).await;
            assert_eq!(purged.map(|counts| counts.disk_files_removed), Some(1));
            let kept = ["unknown", "terminal", "legacy", "torn"].map(|token| file(token).exists());
            assert_eq!(kept, [true, true, false, true]);
            assert_eq!(std::fs::read(file("broken")).unwrap(), b"[{");
        });
    }

    #[test]
    fn s2a_force_purge_refusal_does_not_finalize_a_cancelled_holder() {
        let _root = root();
        run(async {
            let channel = ChannelId::new(6_813_281);
            let handle = ChannelMailboxRegistry::default().handle(channel);
            let token = Arc::new(CancelToken::new());
            handle
                .restore_active_turn(token.clone(), UserId::new(9), MessageId::new(901))
                .await;
            let head = human(281, 7, "H");
            assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
            reserve_unknown(&handle, &head).await;
            assert!(handle.enqueue(human(282, 8, "B"), ctx()).await.enqueued);
            token.cancelled.store(true, Ordering::Relaxed);
            let before = bytes(channel);
            let target = TurnLifecycleTarget {
                provider: Some(ProviderKind::Claude),
                channel_id: Some(channel),
                tmux_name: String::new(),
            };
            let key = format!("claude/{TOKEN}/test:inactive-fixture");
            assert!(
                force_purge_channel_mailbox(None::<&Arc<HealthRegistry>>, &target, Some(&key))
                    .await
                    .is_none()
            );
            assert_eq!(bytes(channel), before);
            assert_eq!(queue(&handle).await, [(281, "H".into()), (282, "B".into())]);
            assert!(Arc::ptr_eq(
                handle.snapshot().await.cancel_token.as_ref().unwrap(),
                &token
            ));
        });
    }

    #[test]
    fn s2a_actorless_purge_preserves_valid_unknown_and_terminal_namespaces() {
        let _root = root();
        run(async {
            let channel = ChannelId::new(6_813_291);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel);
            let head = human(291, 7, "H");
            assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
            reserve_unknown(&handle, &head).await;
            let unknown = bytes(channel);
            let terminal = serde_json::to_vec(&serde_json::json!({
                "version": 1, "items": [], "terminal_identities": [QueueIdentity::of(&head)],
            }))
            .unwrap();
            let base = path(channel).parent().unwrap().parent().unwrap().to_owned();
            let file = |token: &str| base.join(token).join(format!("{}.json", channel.get()));
            let fixtures: [(&str, &[u8]); 4] = [
                ("terminal", &terminal),
                ("broken", b"[{"),
                ("torn", b"{\"version\":1,\"items\":["),
                ("legacy", b"[]"),
            ];
            for (token, body) in fixtures {
                std::fs::create_dir_all(file(token).parent().unwrap()).unwrap();
                std::fs::write(file(token), body).unwrap();
            }
            registry.remove_fixture_for_test(channel);
            assert!(ChannelMailboxRegistry::global_handle(channel).is_none());
            let target = TurnLifecycleTarget {
                provider: Some(ProviderKind::Claude),
                channel_id: Some(channel),
                tmux_name: String::new(),
            };
            let purged = force_purge_channel_mailbox(None::<&Arc<HealthRegistry>>, &target, None)
                .await
                .unwrap();
            assert_eq!(purged.disk_files_removed, 1);
            assert_eq!(bytes(channel), unknown);
            for (token, body) in fixtures.into_iter().filter(|(token, _)| *token != "legacy") {
                assert_eq!(std::fs::read(file(token)).unwrap(), body);
            }
            assert!(!file("legacy").exists());
        });
    }
}
