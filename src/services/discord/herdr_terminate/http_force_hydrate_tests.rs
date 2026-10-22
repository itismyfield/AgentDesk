//! A forced Herdr cancel reaching an actor that has not read its runtime's queue file yet absorbs
//! that file first, so the purge removes and records what it held; a file it may not absorb stays.

use std::os::unix::fs::MetadataExt;

use crate::services::turn_orchestrator::test_support::{fail_queue_saves, queue_save_faults};

use super::*;

const FOREIGN_TOKEN: &str = "foreign-registry";

/// The channel's own queue file as the forced cancel meets it, one state per row; an `-idle` row
/// has no running turn, so nothing wrote through the actor before the cancel.
const ROWS: [&str; 10] = [
    "unhydrated-own-file",
    "unhydrated-own-file-idle",
    "late-62-after-hydrate",
    "hydrate-read-failure",
    "foreign-namespace",
    "foreign-namespace-idle",
    "other-namespace-actor-idle",
    "cleared-untrusted-file",
    "restore-after-purge",
    "hydrated-control",
];

fn hydrate_rig(name: &str) -> Option<(herdr::P11TerminationRig, tokio::runtime::Runtime)> {
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

fn own_file(token: &str) -> std::path::PathBuf {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let dir = root.join("claude").join(token);
    dir.join(format!("{CHANNEL}.json"))
}

fn inode(path: &std::path::Path) -> Option<u64> {
    std::fs::metadata(path).map(|meta| meta.ino()).ok()
}

fn ids(items: &[Item]) -> Vec<u64> {
    items.iter().map(|item| item.0).collect()
}

/// Queues message `id` in `token`'s file through another registry's actor, as a snapshot taken
/// before a restart leaves it; the channel's own actor never read it.
async fn filed_elsewhere(token: &str, id: u64) {
    let registry = ChannelMailboxRegistry::default();
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, token, None);
    let item = intervention(id, test_queue::ORIGINAL);
    let queued = registry.handle(channel()).enqueue(item, persistence).await;
    assert!(queued.enqueued, "message {id} filed for {token}");
}

/// The bound execution, running a dispatched turn unless the row is idle, while the channel's
/// file holds what `row` needs; returns the node and the token whose file holds it.
async fn prepared(rig: &herdr::P11TerminationRig, row: &str) -> (Node, String) {
    let idle = row.ends_with("-idle");
    let dispatch = (!idle).then_some(DISPATCH);
    let mut node = Node::bind(rig, herdr::HostedState::Bound, dispatch).await;
    let own = node.shared.token_hash.clone();
    let context = queue_persistence_context(&node.shared, &ProviderKind::Claude, channel());
    let mailbox = node.shared.mailbox(channel());
    let token = match row {
        "foreign-namespace" | "foreign-namespace-idle" => {
            let key = node.key.replacen(&own, FOREIGN_TOKEN, 1);
            let rename = "UPDATE sessions SET session_key = $2 WHERE session_key = $1";
            node.sql(rename, Some(&key)).await;
            node.key = key;
            filed_elsewhere(FOREIGN_TOKEN, 61).await;
            FOREIGN_TOKEN.to_string()
        }
        "other-namespace-actor-idle" => {
            // A Legacy force purge under another session key's namespace: the actor writes that one.
            let theirs = QueuePersistenceContext::new(&ProviderKind::Claude, OTHER_TOKEN, None);
            let purged = mailbox.try_purge_queue(theirs, false).await.unwrap();
            assert!(!purged.queued_head_refused, "{row}");
            filed_elsewhere(&own, 61).await;
            own
        }
        "hydrate-read-failure" => {
            std::fs::create_dir_all(own_file(&own).parent().unwrap()).unwrap();
            std::fs::write(own_file(&own), "{not a queue").unwrap();
            own
        }
        "cleared-untrusted-file" => {
            filed_elsewhere(&own, 61).await;
            ChannelMailboxRegistry::fail_clear_removals(channel(), false, 2, false);
            ChannelMailboxRegistry::fail_clear_removals(channel(), true, 2, false);
            fail_queue_saves(channel(), 2);
            let cleared = mailbox.clear(context.clone()).await;
            let failed = ["queue_unlink", "marker_unlink", "queue_publish"];
            assert_eq!(
                cleared.persistence.failed, failed,
                "the clear left the file"
            );
            assert_eq!(queue_save_faults(channel(), true), 0);
            own
        }
        "hydrated-control" => {
            filed_elsewhere(&own, 61).await;
            let hydrated = mailbox.try_hydrate_pending_queue_from_disk(context).await;
            assert_eq!(
                hydrated.unwrap().absorbed,
                1,
                "the actor already read the file"
            );
            own
        }
        _ => {
            filed_elsewhere(&own, 61).await;
            own
        }
    };
    if !idle {
        node.start(51, "turn-a", Some(DISPATCH)).await;
    }
    let held = actor(&mailbox).await;
    match row {
        "hydrated-control" => assert_eq!(ids(&held.0), [61], "{row}: memory holds the file"),
        _ => assert_eq!(
            held,
            (vec![], None),
            "{row}: the actor has not read its file"
        ),
    }
    if row != "hydrate-read-failure" {
        assert_eq!(ids(&on_disk(&token).0), [61], "{row}: the file holds 61");
    }
    (node, token)
}

// A forced cancel absorbs its runtime's unread own queue file before the capture; a file it may not
// absorb, or input after the capture, is never purged unseen, and a restart restores nothing purged.
#[test]
fn e07_force_absorbs_unhydrated_own_queue_pg() {
    let Some((rig, runtime)) = hydrate_rig("e07_force_absorbs_unhydrated_own_queue_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let mut rows = 0;
        for row in ROWS {
            clear_disk();
            let (node, token) = prepared(&rig, row).await;
            let own = node.shared.token_hash.clone();
            let path = own_file(&own);
            let unread = std::fs::read(&path).ok();
            let (postlude, at_postlude) = (ran(), Arc::new(Mutex::new(None)));
            let (postlude_flag, postlude_inode, postlude_path) =
                (postlude.clone(), at_postlude.clone(), path.clone());
            crate::services::discord::herdr_terminate::probe_before_postlude(Box::new(move || {
                Box::pin(async move {
                    *postlude_inode.lock().unwrap() = inode(&postlude_path);
                    postlude_flag.store(true, Ordering::SeqCst);
                })
            }));
            let (flag, seen) = (ran(), Arc::new(Mutex::new(None)));
            let (shared, probe_flag, probe_seen, probe_path) = (
                node.shared.clone(),
                flag.clone(),
                seen.clone(),
                path.clone(),
            );
            crate::services::queue::set_pre_purge_probe_for_test(
                channel(),
                Box::new(move || {
                    async move {
                        if row == "late-62-after-hydrate" {
                            Node::enqueue(&shared, 62, "late input").await;
                        }
                        let late = actor(&shared.mailbox(channel())).await;
                        *probe_seen.lock().unwrap() = Some((late, inode(&probe_path)));
                        probe_flag.store(true, Ordering::SeqCst);
                    }
                    .boxed()
                }),
            );
            let (status, reply) = node.cancel().await;
            assert!(
                postlude.load(Ordering::SeqCst),
                "{row}: the postlude ran: {reply}"
            );
            let reached = flag.load(Ordering::SeqCst);
            let probed = seen.lock().unwrap().take();
            let mailbox = node.shared.mailbox(channel());
            let recorded = node.effects(&rig).await.dead_lettered;
            let refused = (
                StatusCode::INTERNAL_SERVER_ERROR,
                result("retired", "retired"),
            );
            let error = reply["error"].as_str().unwrap_or_default().to_string();
            let context = &reply["context"];
            match row {
                "unhydrated-own-file"
                | "unhydrated-own-file-idle"
                | "restore-after-purge"
                | "hydrated-control" => {
                    assert!(reached, "{row}: the purge was asked");
                    assert_eq!(status, StatusCode::OK, "{row}: {reply}");
                    let claims = (
                        &reply["queue_purged"],
                        &reply["queue_dead_lettered_message_ids"],
                    );
                    assert_eq!(claims, (&json!(1), &json!([61])), "{row}: {reply}");
                    assert_eq!(recorded, dead(&[61]), "{row}: the removal is recorded");
                    assert_eq!(actor(&mailbox).await, (vec![], None), "{row}");
                    assert_eq!(on_disk(&token), (vec![], None), "{row}");
                    assert_eq!(
                        restarted(&token).await,
                        [],
                        "{row}: a restart restores none"
                    );
                }
                "late-62-after-hydrate" => {
                    assert!(reached, "{row}: input arrived after the capture");
                    assert_eq!(termination(status, &reply), refused, "{row}: {reply}");
                    let (late, _) = probed.clone().unwrap();
                    assert_eq!(ids(&late.0), [61, 62], "{row}: both reached the actor");
                    assert_eq!(actor(&mailbox).await, late, "{row}: the actor keeps both");
                    assert_eq!(on_disk(&token), late, "{row}: so does its file");
                    assert!(recorded.is_empty(), "{row}: nothing recorded: {recorded:?}");
                    assert_eq!(ids(&restarted(&token).await), [61, 62], "{row}");
                }
                "hydrate-read-failure" | "other-namespace-actor-idle" => {
                    assert!(!reached, "{row}: no capture or purge was asked: {reply}");
                    assert_eq!(termination(status, &reply), refused, "{row}: {reply}");
                    assert!(error.contains("queue hydrate: "), "{row}: {reply}");
                    assert_eq!(
                        context["queue_dead_lettered_message_ids"],
                        json!([]),
                        "{row}"
                    );
                    assert_ne!(context["queue_loss_recorded"], json!(true), "{row}");
                    assert_eq!(std::fs::read(&path).ok(), unread, "{row}: the file is kept");
                    assert_eq!(actor(&mailbox).await, (vec![], None), "{row}");
                    assert!(recorded.is_empty(), "{row}: nothing recorded: {recorded:?}");
                }
                "foreign-namespace" | "foreign-namespace-idle" => {
                    let why = match row {
                        "foreign-namespace" => "the captured namespace is not the one",
                        _ => "the queue file differs",
                    };
                    assert!(reached, "{row}: the purge was asked");
                    assert_eq!(termination(status, &reply), refused, "{row}: {reply}");
                    assert!(error.contains(why), "{row}: {reply}");
                    let (held, _) = probed.clone().unwrap();
                    assert_eq!(held, (vec![], None), "{row}: nothing absorbed");
                    assert_eq!(actor(&mailbox).await, (vec![], None), "{row}");
                    assert_eq!(
                        ids(&on_disk(&token).0),
                        [61],
                        "{row}: the foreign file stays"
                    );
                    assert_eq!(on_disk(&own), (vec![], None), "{row}: no own file written");
                    assert!(recorded.is_empty(), "{row}: nothing recorded: {recorded:?}");
                }
                _ => {
                    assert!(reached, "{row}: the purge was asked");
                    assert_eq!(status, StatusCode::OK, "{row}: {reply}");
                    let claims = (
                        &reply["queue_purged"],
                        &reply["queue_dead_lettered_message_ids"],
                    );
                    assert_eq!(claims, (&json!(0), &json!([])), "{row}: {reply}");
                    let (held, _) = probed.clone().unwrap();
                    assert_eq!(
                        held,
                        (vec![], None),
                        "{row}: the distrusted file is not absorbed"
                    );
                    assert_eq!(actor(&mailbox).await, (vec![], None), "{row}");
                    assert!(recorded.is_empty(), "{row}: nothing recorded: {recorded:?}");
                }
            }
            if row == "hydrated-control" {
                let (_, at_purge) = probed.unwrap();
                let before = *at_postlude.lock().unwrap();
                assert!(before.is_some(), "{row}: the file existed at the postlude");
                assert_eq!(
                    at_purge, before,
                    "{row}: nothing rewrote it before the purge"
                );
            }
            if row == "restore-after-purge" {
                let context =
                    queue_persistence_context(&node.shared, &ProviderKind::Claude, channel());
                let item = intervention(61, test_queue::ORIGINAL);
                let merged = mailbox
                    .merge_restored_queue_items(vec![item], context)
                    .await;
                assert_eq!(merged.absorbed, 0, "{row}: the purged item is not restored");
                assert_eq!(
                    actor(&mailbox).await,
                    (vec![], None),
                    "{row}: nothing to dispatch"
                );
                assert_eq!(on_disk(&token), (vec![], None), "{row}");
            }
            node.finish().await;
            rows += 1;
        }
        assert_eq!(rows, ROWS.len());
    });
}
