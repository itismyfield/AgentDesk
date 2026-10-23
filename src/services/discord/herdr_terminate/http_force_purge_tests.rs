//! A forced Herdr cancel purges only the queue it captured: input that reached the actor after the
//! capture survives the request, and what the purge removed is exactly what it records.

use crate::services::cluster::attachment_transfer::uploads::Upload;
use crate::services::discord::queue_persistence_context;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, ChannelMailboxRegistry, DispatchLease, QueuePersistenceContext,
    load_channel_pending_dispatch_marker, load_channel_pending_queue_for_tests,
};

use super::super::Node;
use super::*;

const OTHER_TOKEN: &str = "other-registry";

/// Input that reaches the actor between the capture and the purge, one kind per row.
const LATE: [&str; 5] = ["new-item", "merged", "upload", "generation", "reservation"];

/// A forced cancel the purge serves without a race, and the states around it.
const RECEIPT: [&str; 4] = ["plain", "other-token", "re-inflow", "published-error"];

fn rig(name: &str) -> Option<(herdr::P11TerminationRig, tokio::runtime::Runtime)> {
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

/// What a queued item carries that a purge must not lose: id, text, sources, uploads, generation.
type Item = (u64, String, Vec<u64>, usize, u64);

fn view(items: &[Intervention]) -> Vec<Item> {
    let item = |i: &Intervention| {
        let sources = i.source_message_ids.iter().map(|id| id.get()).collect();
        let uploads = i.pending_uploads.len();
        (
            i.message_id.get(),
            i.text.clone(),
            sources,
            uploads,
            i.queued_generation,
        )
    };
    items.iter().map(item).collect()
}

/// The actor's queue and the message its dispatch reservation holds.
async fn actor(handle: &ChannelMailboxHandle) -> (Vec<Item>, Option<u64>) {
    let snapshot = handle.snapshot().await;
    let reserved = snapshot.pending_user_dispatch.map(|id| id.get());
    (view(&snapshot.intervention_queue), reserved)
}

/// The queue file and dispatch marker of `token`'s namespace for the channel.
fn on_disk(token: &str) -> (Vec<Item>, Option<u64>) {
    let (queue, _) = load_channel_pending_queue_for_tests(&ProviderKind::Claude, token, channel());
    let marker = load_channel_pending_dispatch_marker(&ProviderKind::Claude, token, channel());
    (view(&queue), marker.map(|(item, _)| item.message_id.get()))
}

/// The queue a fresh actor of `token`'s namespace hydrates from disk, as a restart restores it.
async fn restarted(token: &str) -> Vec<Item> {
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(channel());
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, token, None);
    let hydrated = handle
        .try_hydrate_pending_queue_from_disk(persistence)
        .await;
    assert!(hydrated.unwrap().persistence_error.is_none());
    view(&handle.snapshot().await.intervention_queue)
}

/// Each row starts from an empty pending-queue root.
fn clear_disk() {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

fn mergeable(id: u64, text: &str) -> Intervention {
    Intervention {
        merge_consecutive: true,
        ..intervention(id, text)
    }
}

/// The bound execution runs a dispatched turn with message 61 queued behind it; with `metadata`
/// the queue file already records a submitted source, so each queue write publishes a snapshot.
async fn running(rig: &herdr::P11TerminationRig, metadata: bool) -> Node {
    let node = Node::bind(rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
    if metadata {
        published_snapshot_with_metadata(&node.shared.token_hash);
    }
    node.start(51, "turn-a", Some(DISPATCH)).await;
    let context = queue_persistence_context(&node.shared, &ProviderKind::Claude, channel());
    let queue = vec![mergeable(61, test_queue::ORIGINAL)];
    node.shared
        .mailbox(channel())
        .replace_queue(queue, context)
        .await;
    node
}

/// Applies `row`'s late input to the channel's actor; a reservation's lease is kept by the caller.
async fn arrive(shared: &Arc<SharedData>, row: &str) -> Option<Arc<DispatchLease>> {
    let mailbox = shared.mailbox(channel());
    let context = queue_persistence_context(shared, &ProviderKind::Claude, channel());
    let replace = |edit: fn(&mut Intervention)| {
        let mut item = mergeable(61, test_queue::ORIGINAL);
        edit(&mut item);
        mailbox.replace_queue(vec![item], context.clone())
    };
    match row {
        "new-item" => Node::enqueue(shared, 62, "late input").await,
        "merged" => {
            let late = mailbox.enqueue(mergeable(62, "late merged input"), context.clone());
            assert!(late.await.merged, "the late input joins the captured item");
        }
        "upload" => {
            let upload =
                |item: &mut Intervention| item.pending_uploads = vec![Upload::Local("late".into())];
            replace(upload).await;
        }
        "generation" => replace(|item| item.queued_generation += 1).await,
        "reservation" => {
            let taken = mailbox.take_next_soft(context.clone()).await;
            let id = taken.intervention.map(|item| item.message_id.get());
            assert_eq!(
                id,
                Some(61),
                "the captured item moved to the dispatch reservation"
            );
            return taken.dispatch_lease;
        }
        _ => unreachable!("unknown row {row}"),
    }
    None
}

// Input that changed the actor's queue after the capture is never purged with it: the request
// fails, the actor, its file and a restart keep the queue the input left, and nothing is recorded.
#[test]
fn e07_snapshot_purge_refuses_late_durable_input_pg() {
    let Some((rig, runtime)) = rig("e07_snapshot_purge_refuses_late_durable_input_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let mut rows = 0;
        for row in LATE {
            clear_disk();
            let node = running(&rig, false).await;
            let token = node.shared.token_hash.clone();
            let (flag, seen) = (ran(), Arc::new(Mutex::new(None)));
            let (shared, probe_flag, probe_seen) =
                (node.shared.clone(), flag.clone(), seen.clone());
            crate::services::queue::set_pre_purge_probe_for_test(
                channel(),
                Box::new(move || {
                    async move {
                        let lease = arrive(&shared, row).await;
                        let late = actor(&shared.mailbox(channel())).await;
                        *probe_seen.lock().unwrap() = Some((late, lease));
                        probe_flag.store(true, Ordering::SeqCst);
                    }
                    .boxed()
                }),
            );
            let (status, reply) = node.cancel().await;
            assert!(
                flag.load(Ordering::SeqCst),
                "{row}: input arrived after the capture"
            );
            let (late, _lease) = seen.lock().unwrap().take().unwrap();
            let kept = actor(&node.shared.mailbox(channel())).await;
            assert_eq!(kept, late, "{row}: the actor keeps the late queue: {reply}");
            assert_eq!(on_disk(&token), late, "{row}: so does its own file");
            let recorded = node.effects(&rig).await.dead_lettered;
            assert!(
                recorded.is_empty(),
                "{row}: nothing recorded as removed: {recorded:?}"
            );
            assert_eq!(
                termination(status, &reply),
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    result("retired", "retired")
                ),
                "{row}: {reply}"
            );
            let restored: Vec<u64> = restarted(&token).await.iter().map(|item| item.0).collect();
            let live: Vec<u64> = late.0.iter().map(|item| item.0).chain(late.1).collect();
            assert_eq!(
                restored, live,
                "{row}: a restart restores the queue and any reservation"
            );
            node.finish().await;
            rows += 1;
        }
        assert_eq!(rows, LATE.len());
    });
}

// Without a race the purge records exactly what it removed and leaves other namespaces and later
// input alone; a save error that still published the removal records it and fails the request.
#[test]
fn e07_snapshot_purge_records_exact_receipt_pg() {
    let Some((rig, runtime)) = rig("e07_snapshot_purge_records_exact_receipt_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let mut rows = 0;
        for row in RECEIPT {
            clear_disk();
            let node = running(&rig, row == "published-error").await;
            let token = node.shared.token_hash.clone();
            let (flag, other) = (ran(), Arc::new(Mutex::new(None)));
            let (shared, probe_flag, probe_other) =
                (node.shared.clone(), flag.clone(), other.clone());
            match row {
                "plain" => flag.store(true, Ordering::SeqCst),
                "other-token" => crate::services::queue::set_pre_purge_probe_for_test(
                    channel(),
                    Box::new(move || {
                        async move {
                            let registry = ChannelMailboxRegistry::default();
                            let handle = registry.handle(channel());
                            let theirs = QueuePersistenceContext::new(
                                &ProviderKind::Claude,
                                OTHER_TOKEN,
                                None,
                            );
                            let queue = vec![intervention(99, "the other actor's input")];
                            handle.replace_queue(queue, theirs).await;
                            *probe_other.lock().unwrap() = Some((registry, handle));
                            probe_flag.store(true, Ordering::SeqCst);
                        }
                        .boxed()
                    }),
                ),
                "re-inflow" => {
                    crate::services::turn_cancel_queue_guard::set_pre_record_probe_for_test(
                        channel(),
                        Box::new(move || {
                            async move {
                                Node::enqueue(&shared, 61, "the same message sent again").await;
                                Node::enqueue(&shared, 63, "input after the purge").await;
                                probe_flag.store(true, Ordering::SeqCst);
                            }
                            .boxed()
                        }),
                    )
                }
                // Armed right before the purge, so no earlier queue write takes the fault.
                "published-error" => crate::services::queue::set_pre_purge_probe_for_test(
                    channel(),
                    Box::new(move || {
                        async move {
                            use crate::services::turn_orchestrator::checked_purge as purge;
                            purge::fail_next_publication_for_test(channel());
                            probe_flag.store(true, Ordering::SeqCst);
                        }
                        .boxed()
                    }),
                ),
                _ => unreachable!("unknown row {row}"),
            }
            let (status, reply) = node.cancel().await;
            assert!(flag.load(Ordering::SeqCst), "{row}: the probe ran");
            let mailbox = node.shared.mailbox(channel());
            let recorded = node.effects(&rig).await.dead_lettered;
            assert_eq!(
                recorded,
                dead(&[61]),
                "{row}: the removed item is recorded: {reply}"
            );
            match row {
                "plain" | "other-token" => {
                    assert_eq!(status, StatusCode::OK, "{row}: {reply}");
                    let claims = (
                        &reply["queue_purged"],
                        &reply["queue_dead_lettered_message_ids"],
                    );
                    assert_eq!(claims, (&json!(1), &json!([61])), "{row}: {reply}");
                    assert_eq!(reply["queue_loss_recorded"], Value::Null, "{row}");
                    assert_eq!(actor(&mailbox).await, (vec![], None), "{row}");
                    assert_eq!(on_disk(&token), (vec![], None), "{row}");
                }
                "re-inflow" => {
                    assert_eq!(status, StatusCode::OK, "{row}: {reply}");
                    let (queue, _) = actor(&mailbox).await;
                    let ids: Vec<u64> = queue.iter().map(|item| item.0).collect();
                    assert_eq!(ids, [61, 63], "{row}: input after the purge stays");
                    assert_eq!(on_disk(&token).0, queue, "{row}");
                    assert_eq!(
                        reply["queue_dead_lettered_message_ids"],
                        json!([61]),
                        "{row}"
                    );
                }
                _ => {
                    assert_eq!(
                        termination(status, &reply),
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            result("retired", "retired")
                        ),
                        "{row}: {reply}"
                    );
                    let context = &reply["context"];
                    assert_eq!(
                        context["queue_dead_lettered_message_ids"],
                        json!([61]),
                        "{reply}"
                    );
                    assert_ne!(context["queue_loss_recorded"], json!(true), "{reply}");
                    assert_eq!(actor(&mailbox).await, (vec![], None), "{row}");
                    assert_eq!(on_disk(&token), (vec![], None), "{row}");
                }
            }
            if row != "re-inflow" {
                assert_eq!(
                    restarted(&token).await,
                    [],
                    "{row}: a restart restores nothing removed"
                );
            }
            if row == "other-token" {
                let (_registry, handle) = other.lock().unwrap().take().unwrap();
                let theirs = vec![(99, "the other actor's input".to_string(), vec![99], 0, 0)];
                let generation = |queue: Vec<Item>| -> Vec<Item> {
                    queue
                        .into_iter()
                        .map(|(id, text, sources, uploads, _)| (id, text, sources, uploads, 0))
                        .collect()
                };
                assert_eq!(
                    generation(actor(&handle).await.0),
                    theirs,
                    "the other actor keeps its queue"
                );
                assert_eq!(generation(on_disk(OTHER_TOKEN).0), theirs, "and its file");
                assert_eq!(
                    generation(restarted(OTHER_TOKEN).await),
                    theirs,
                    "which a restart restores"
                );
            }
            node.finish().await;
            rows += 1;
        }
        assert_eq!(rows, RECEIPT.len());
    });
}

/// `token`'s queue file for the channel holding only a submitted-source record.
fn published_snapshot_with_metadata(token: &str) {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let dir = root.join("claude").join(token);
    std::fs::create_dir_all(&dir).unwrap();
    let source = json!({"message_id": 1, "queued_generation": 1});
    let terminal = json!({"primary": 1, "queued_generation": 1, "sources": [source]});
    let snapshot = json!({"version": 1, "items": [], "terminal_identities": [terminal]});
    std::fs::write(dir.join(format!("{CHANNEL}.json")), snapshot.to_string()).unwrap();
}

#[path = "http_force_hydrate_tests.rs"]
mod hydrate;
