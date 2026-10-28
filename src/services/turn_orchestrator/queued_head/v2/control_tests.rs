use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, ChannelMailboxRegistry, QueuePersistenceContext, queued_head::compat,
    registry_purge::MailboxPurgeOutcome,
};
use serde_json::{Value, json};
use std::path::Path;

fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "v2-control", None)
}

fn item(id: u64) -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":7272,
        "source_message_ids":[id],"queued_generation":5,"text":format!("input {id}")}))
    .unwrap()
}

fn intervention(id: u64) -> Intervention {
    pending_queue_item_to_intervention(item(id), SystemTime::now(), Instant::now())
}

fn attempt(id: u64) -> AttemptIdentity {
    AttemptIdentity {
        nonce: format!("nonce-{id}"),
        provider: "codex".into(),
        target: TargetIdentity {
            host: "tmux".into(),
            owner: "control-pane".into(),
            root: "/test".into(),
            execution_nonce: "execution".into(),
        },
        source: AttemptSource::Existing {
            session_id: "native".into(),
            transcript: "/test/rollout".into(),
            file: [1, 2],
            offset: 0,
        },
    }
}

fn run(work: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(work);
}

/// An actor holding a V2 owner whose queue is an attempted head 100 and untried `rest`.
async fn seeded(
    registry: &ChannelMailboxRegistry,
    channel: u64,
    rest: &[u64],
) -> (ChannelMailboxHandle, PathBuf) {
    let channel = ChannelId::new(channel);
    let path =
        super::super::super::pending_queue_file_path(&ctx().provider, &ctx().token_hash, channel)
            .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let handle = registry.handle(channel);
    handle
        .v2_adopt(path.clone(), "control".into())
        .await
        .unwrap();
    let rest = rest.to_vec();
    handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            owner.enqueue_fixture(&item(100)).map_err(String::from)?;
            let payload = PreparedPayload {
                frame: "frame".into(),
                digest: "digest".into(),
                attribution: json!({}),
            };
            let key = owner.reserve(attempt(100), payload)?;
            drop(owner.arm(&key, 0)?);
            for id in rest {
                owner.enqueue_fixture(&item(id)).map_err(String::from)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    (handle, path)
}

async fn v2(handle: &ChannelMailboxHandle) -> Value {
    handle
        .v2_request(|slot| slot.as_ref().ok_or("no owner")?.json())
        .await
        .unwrap()
}

fn ids(snapshot: &Value) -> Vec<u64> {
    snapshot["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["message_id"].as_u64().unwrap())
        .collect()
}

/// No V1 queue beside the owner, and disk holds exactly what the owner committed.
async fn single_writer(handle: &ChannelMailboxHandle, path: &Path, step: &str) -> Value {
    assert!(
        handle.snapshot().await.intervention_queue.is_empty(),
        "{step}: V1 queue stays empty"
    );
    let owner = v2(handle).await;
    let disk: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(disk, owner, "{step}: disk is the owner's commit");
    owner
}

#[test]
fn v2_queue_arms_reach_the_owner_or_refuse_without_a_v1_writer() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let (handle, path) = seeded(&registry, 7_272_401, &[200, 300]).await;
        let marker_path = path.with_extension("dispatch");
        let marker = std::fs::read(&marker_path).ok();
        let context = ctx();
        let channel = ChannelId::new(7_272_401);
        crate::services::turn_orchestrator::pending_queue_persistence::save_channel_pending_dispatch_marker(
            &context.provider, &context.token_hash, channel, &intervention(100), None,
        )
        .unwrap();
        assert!(marker.is_none());
        let marker = std::fs::read(&marker_path).expect("a real dispatch marker");
        let held = |snapshot: &Value| snapshot["head_injection"]["identity"]["primary"] == 100;

        assert!(handle.has_pending_soft_queue(ctx()).await.has_pending);
        let taken = handle.take_next_soft(ctx()).await;
        assert!(taken.intervention.is_none(), "withheld dispatches nothing");
        assert_eq!(
            (taken.has_more, taken.queue_len_after),
            (true, 3),
            "withheld is not an empty queue"
        );
        assert_eq!(
            ids(&single_writer(&handle, &path, "take").await),
            [100, 200, 300]
        );

        let enqueued = handle.enqueue(intervention(400), ctx()).await;
        assert!(enqueued.enqueued && enqueued.persistence_error.is_none());
        let after = single_writer(&handle, &path, "enqueue").await;
        assert_eq!(ids(&after), [100, 200, 300, 400]);

        let cancelled = handle
            .cancel_queued_primary_message(MessageId::new(300), ctx())
            .await;
        assert_eq!(cancelled.removed.map(|i| i.message_id.get()), Some(300));
        let head = handle
            .cancel_queued_primary_message(MessageId::new(100), ctx())
            .await;
        assert!(
            head.removed.is_none(),
            "the attempted head is never cancelled"
        );
        let after = single_writer(&handle, &path, "cancel").await;
        assert_eq!(ids(&after), [100, 200, 400]);
        assert!(held(&after));

        let requeued = handle
            .requeue_front_inner(intervention(500), ctx(), None)
            .await;
        assert!(!requeued.enqueued && requeued.persistence_error.is_some());
        assert!(
            !handle
                .abandon_pending_dispatch(MessageId::new(200), ctx())
                .await
        );
        for restored in [
            handle
                .hydrate_pending_queue_from_disk_or_refused(ctx())
                .await,
            handle
                .merge_restored_queue_items_or_refused(vec![intervention(600)], ctx())
                .await,
            handle
                .merge_restored_dispatch_marker_or_refused(intervention(700), None, ctx())
                .await,
        ] {
            let restored = restored.expect("an answer, not a closed-actor refusal");
            assert_eq!(restored.absorbed, 0);
            assert!(
                restored.persistence_error.is_some(),
                "a refusal is never a success"
            );
            assert_eq!(
                restored.queue_len_after, 3,
                "a refusal is not an empty queue"
            );
        }
        assert_eq!(handle.restart_drain(ctx()).await.queued_count, 3);
        assert_eq!(
            std::fs::read(&marker_path).unwrap(),
            marker,
            "refusals keep the marker"
        );
        assert_eq!(
            ids(&single_writer(&handle, &path, "refusals").await),
            [100, 200, 400]
        );

        let cleared = handle.clear(ctx()).await;
        let mut discarded: Vec<u64> = cleared
            .discarded_message_ids
            .iter()
            .map(|id| id.get())
            .collect();
        discarded.sort_unstable();
        assert_eq!(discarded, [200, 400], "only untried items are discarded");
        assert!(cleared.persistence.failed.is_empty());
        let after = single_writer(&handle, &path, "clear").await;
        assert_eq!(ids(&after), [100]);
        assert!(held(&after), "the attempted head stays in custody");
        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(matches!(
            compat::inspect(&bytes),
            Ok(compat::SnapshotRead::ReadOnlyV2(_))
        ));
    });
}

#[test]
fn a_full_v2_queue_refuses_enqueue_without_dropping_the_attempted_head() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let rest: Vec<u64> = (201..230).collect();
        let (handle, path) = seeded(&registry, 7_272_402, &rest).await;
        let refused = handle.enqueue(intervention(999), ctx()).await;
        assert!(
            !refused.enqueued,
            "a full queue is never reported as queued"
        );
        assert!(refused.persistence_error.is_some());
        let after = single_writer(&handle, &path, "full").await;
        assert_eq!(ids(&after).len(), 30);
        assert_eq!(ids(&after)[0], 100);
        assert!(!ids(&after).contains(&999));
    });
}

#[test]
fn a_closed_actor_refuses_v2_requests_and_a_successor_readopts_the_same_bytes() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let (handle, path) = seeded(&registry, 7_272_403, &[200]).await;
        let before = std::fs::read(&path).unwrap();
        let channel = ChannelId::new(7_272_403);
        assert_eq!(
            registry.remove_idle_entry(channel).await,
            MailboxPurgeOutcome::Removed
        );
        assert!(
            handle.v2_request(|slot| Ok(slot.is_some())).await.is_err(),
            "a closed actor serves no V2 request"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let successor = registry.handle(channel);
        successor
            .v2_adopt(path.clone(), "control".into())
            .await
            .unwrap();
        assert_eq!(
            ids(&single_writer(&successor, &path, "readopt").await),
            [100, 200]
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "re-adoption rewrites nothing"
        );
    });
}

/// A running turn on `channel`, started before the owner holds the queue it seeds.
async fn running(
    registry: &ChannelMailboxRegistry,
    channel: u64,
    fault: bool,
) -> (
    ChannelMailboxHandle,
    PathBuf,
    std::sync::Arc<crate::services::turn_orchestrator::TurnFinishedSignal>,
) {
    let token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
    let owner = poise::serenity_prelude::UserId::new(7);
    let idle = registry.handle(ChannelId::new(channel));
    assert!(idle.try_start_turn(token, owner, MessageId::new(900)).await);
    let (handle, path) = seeded(registry, channel, &[200]).await;
    if fault {
        handle
            .v2_request(|slot| {
                slot.as_mut().ok_or("no owner")?.fault = super::super::Fault::Before;
                Ok(())
            })
            .await
            .unwrap();
    }
    let signal = crate::services::turn_orchestrator::turn_finished_signal(ChannelId::new(channel));
    (handle, path, signal)
}

async fn finished(signal: &crate::services::turn_orchestrator::TurnFinishedSignal) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(1), signal.wait())
        .await
        .is_ok()
}

#[test]
fn a_v2_clear_still_releases_the_running_turn_when_asked() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let (handle, path, signal) = running(&registry, 7_272_404, false).await;
        let cleared = handle.clear(ctx()).await;
        assert!(
            cleared.removed_token.is_some(),
            "the running turn is released"
        );
        assert!(handle.snapshot().await.cancel_token.is_none());
        assert_eq!(ids(&single_writer(&handle, &path, "clear").await), [100]);
        assert!(
            finished(&signal).await,
            "a waiter that subscribes after the clear still wakes"
        );

        let (kept, _, signal) = running(&registry, 7_272_405, false).await;
        let waiting = kept.discard_waiting_or_refused(ctx()).await.unwrap();
        assert!(waiting.removed_token.is_none());
        assert!(
            !finished(&signal).await,
            "a clear that keeps the turn signals nothing"
        );

        let (failing, _, signal) = running(&registry, 7_272_406, true).await;
        let cleared = failing.clear(ctx()).await;
        assert!(cleared.persistence.failed.contains(&"queue_publish"));
        assert!(cleared.removed_token.is_some());
        assert!(
            finished(&signal).await,
            "a released turn signals even when the queue did not publish"
        );
    });
}

/// A V1 enqueue that saw Pending is acknowledged on its retry; a fresh custody per retry would
/// call the recovered input refused or a duplicate.
#[test]
fn enqueue_keeps_one_custody_across_pending_retries() {
    use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(7_272_691);
        let path = super::super::super::pending_queue_file_path(
            &ctx().provider,
            &ctx().token_hash,
            channel,
        )
        .unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let handle = registry.handle(channel);
        handle
            .v2_adopt(path.clone(), "control".into())
            .await
            .unwrap();
        let watch = Watch::new(
            &path,
            Plan {
                fail: vec![Step::ParentSync],
                ..Default::default()
            },
        );
        let first = handle.enqueue(intervention(500), ctx()).await;
        assert!(!first.enqueued && first.persistence_error.is_some());
        drop(watch);
        let retry = handle.enqueue(intervention(500), ctx()).await;
        assert!(
            retry.enqueued && retry.persistence_error.is_none(),
            "{:?} {:?}",
            retry.refusal_reason,
            retry.persistence_error
        );
        let again = handle.enqueue(intervention(500), ctx()).await;
        assert!(!again.enqueued && again.refusal_reason.is_some());
        let snapshot = v2(&handle).await;
        assert_eq!(ids(&snapshot), [500]);
        assert_eq!(
            snapshot["admission_receipts"].as_array().map(Vec::len),
            Some(1)
        );
    });
}

/// A synthetic id can name several logical inputs, so each enqueue is its own admission: a second
/// input under the same id, and one after the first has run, are both queued.
#[test]
fn synthetic_inputs_sharing_an_id_are_separate_admissions() {
    use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
    let _root = TestRuntimeRootGuard::new();
    let synthetic = |text: &str| {
        let id = 8_000_000_000_000_007_272_u64;
        let item: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
            "message_id":id,"channel_id":7272,"source_message_ids":[id],"queued_generation":5,
            "text":text}))
        .unwrap();
        pending_queue_item_to_intervention(item, SystemTime::now(), Instant::now())
    };
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(7_272_692);
        let path = super::super::super::pending_queue_file_path(
            &ctx().provider,
            &ctx().token_hash,
            channel,
        )
        .unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let handle = registry.handle(channel);
        handle
            .v2_adopt(path.clone(), "control".into())
            .await
            .unwrap();
        let watch = Watch::new(
            &path,
            Plan {
                fail: vec![Step::ParentSync],
                ..Default::default()
            },
        );
        let first = handle.enqueue(synthetic("A"), ctx()).await;
        assert!(!first.enqueued && first.persistence_error.is_some());
        drop(watch);
        let retry = handle.enqueue(synthetic("A"), ctx()).await;
        assert!(!retry.enqueued, "a retry never admits a second copy");
        handle
            .v2_request(|slot| slot.as_mut().ok_or("no owner")?.recover())
            .await
            .unwrap();
        let texts = |snapshot: &Value| -> Vec<String> {
            let items = snapshot["items"].as_array().unwrap();
            items
                .iter()
                .map(|i| i["text"].as_str().unwrap().into())
                .collect()
        };
        let snapshot = v2(&handle).await;
        assert_eq!(texts(&snapshot), ["A"]);
        assert_eq!(snapshot["admission_receipts"].as_array().unwrap().len(), 1);

        let second = handle.enqueue(synthetic("B"), ctx()).await;
        assert!(second.enqueued, "{:?}", second.refusal_reason);
        handle
            .v2_request(|slot| {
                let owner = slot.as_mut().ok_or("no owner")?;
                let payload = PreparedPayload {
                    frame: "frame".into(),
                    digest: "digest".into(),
                    attribution: json!({}),
                };
                let key = owner.reserve(attempt(7272), payload)?;
                drop(owner.arm(&key, 0)?);
                owner.settle(&key, true, 1)
            })
            .await
            .unwrap();
        let after = handle.enqueue(synthetic("C"), ctx()).await;
        assert!(after.enqueued, "{:?}", after.refusal_reason);
        assert_eq!(texts(&v2(&handle).await), ["B", "C"]);
    });
}
