use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::checked_purge::ExpectedQueue;
use crate::services::turn_orchestrator::pending_queue_persistence::{
    pending_queue_item_to_intervention, save_channel_pending_dispatch_marker,
};
use crate::services::turn_orchestrator::queued_head::{
    PersistedHeadInjection, canonical_item, compat, mend::HeadAttempt, snapshot_json,
};
use crate::services::turn_orchestrator::{
    ActiveTurnKind, ChannelMailboxHandle, Intervention, TurnAdmissionOrder,
    registry_purge::MailboxPurgeOutcome,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

const NAMESPACE: &str = "projection";

fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "v2-projection", None)
}

fn item(id: u64) -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":7272,
        "source_message_ids":[id],"queued_generation":5,"text":format!("input {id}")}))
    .unwrap()
}

fn intervention(id: u64) -> Intervention {
    let now = (std::time::SystemTime::now(), std::time::Instant::now());
    pending_queue_item_to_intervention(item(id), now.0, now.1)
}

fn path(channel: u64) -> PathBuf {
    let ctx = ctx();
    let path = pending_queue_file_path(&ctx.provider, &ctx.token_hash, ChannelId::new(channel));
    let path = path.unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    path
}

fn seed(channel: u64, raw: &str) -> PathBuf {
    let file = path(channel);
    std::fs::write(&file, raw).unwrap();
    file
}

/// A legacy envelope; `attempted` records a head that may have been sent.
fn envelope(items: Vec<PendingQueueItem>, attempted: bool) -> String {
    let head = attempted.then(|| {
        let original = items[0].clone();
        let now = (std::time::SystemTime::now(), std::time::Instant::now());
        let intervention = pending_queue_item_to_intervention(original.clone(), now.0, now.1);
        PersistedHeadInjection {
            identity: sources::identity(&original),
            original: canonical_item(&intervention, ChannelId::new(7272)),
            phase: HeadPhase::MayHaveSent,
            attempt: Some(HeadAttempt {
                nonce: "legacy-nonce".into(),
                transcript: "/legacy/session.jsonl".into(),
                session_id: "legacy-session".into(),
                file: (3, 4),
                offset: 9,
            }),
        }
    });
    let meta = crate::services::turn_orchestrator::queued_head::SnapshotMeta {
        head_injection: head,
        terminal_identities: vec![sources::identity(&item(99))],
    };
    snapshot_json(items, Some(&meta)).unwrap()
}

fn run(work: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(work);
}

/// A V2 owner adopted on `channel` whose queue is `items`, plus one retained input when asked.
async fn owned(
    registry: &ChannelMailboxRegistry,
    channel: u64,
    items: &[u64],
    retained: bool,
) -> (ChannelMailboxHandle, PathBuf) {
    let file = path(channel);
    let handle = registry.handle(ChannelId::new(channel));
    handle
        .v2_adopt(file.clone(), NAMESPACE.into())
        .await
        .unwrap();
    let items = items.to_vec();
    handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            for id in items {
                owner
                    .enqueue_fixture(&item(id))
                    .map_err(|e| format!("{e:?}"))?;
            }
            if retained {
                sources::migrate_parked(owner, &item(300), false).map_err(|e| format!("{e:?}"))?;
            }
            Ok(())
        })
        .await
        .unwrap();
    (handle, file)
}

/// Arms the first queued item as the attempted head.
async fn attempt_head(handle: &ChannelMailboxHandle) {
    handle
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let attempt = AttemptIdentity {
                nonce: "nonce".into(),
                provider: "codex".into(),
                target: TargetIdentity {
                    host: "tmux".into(),
                    owner: "projection-pane".into(),
                    root: "/test".into(),
                    execution_nonce: "execution".into(),
                },
                source: AttemptSource::Existing {
                    session_id: "native".into(),
                    transcript: "/test/rollout".into(),
                    file: [1, 2],
                    offset: 0,
                },
            };
            let payload = PreparedPayload {
                frame: "frame".into(),
                digest: "digest".into(),
                attribution: json!({}),
            };
            let key = owner.reserve(attempt, payload)?;
            drop(owner.arm(&key, 0)?);
            Ok(())
        })
        .await
        .unwrap();
}

async fn queued(handle: &ChannelMailboxHandle) -> (Vec<u64>, bool) {
    handle
        .v2_request(|slot| {
            let owner = slot.as_ref().ok_or("no owner")?;
            let ids = owner.snapshot.items.iter().map(|i| i.message_id).collect();
            Ok((ids, owner.snapshot.head_injection.is_some()))
        })
        .await
        .unwrap()
}

#[test]
fn boot_plan_orders_phases_adopts_metadata_only_and_holds_what_does_not_migrate() {
    let _root = TestRuntimeRootGuard::new();
    let shapes = [
        (7_272_601, serde_json::to_string(&vec![item(100)]).unwrap()),
        (7_272_602, envelope(vec![item(100)], false)),
        (7_272_603, envelope(vec![], false)),
        (7_272_604, envelope(vec![item(100)], true)),
        (7_272_605, "{\"version\":3,\"items\":[]}".into()),
        (7_272_606, "{\"version\":1,\"items\":[".into()),
    ];
    let mut files: Vec<(PathBuf, Vec<u8>)> = shapes
        .iter()
        .map(|(channel, raw)| (seed(*channel, raw), raw.clone().into_bytes()))
        .collect();
    let ctx = ctx();
    let attempted = ChannelId::new(7_272_604);
    save_channel_pending_dispatch_marker(
        &ctx.provider,
        &ctx.token_hash,
        attempted,
        &intervention(100),
        None,
    )
    .unwrap();
    for (channel, namespace) in [(7_272_607, NAMESPACE), (7_272_608, "another scope")] {
        let mut owner = Owner::adopt(path(channel), namespace.into()).unwrap();
        sources::migrate_parked(&mut owner, &item(300), false).unwrap();
        files.push((path(channel), std::fs::read(path(channel)).unwrap()));
    }

    let steps = plan(&ctx, NAMESPACE);
    let channels = |want: fn(&Step) -> bool| -> Vec<u64> {
        let mut ids: Vec<u64> = steps
            .iter()
            .filter(|s| want(s))
            .map(|s| s.channel().get())
            .collect();
        ids.sort_unstable();
        ids
    };
    assert_eq!(
        channels(|s| matches!(s, Step::Adopt(_))),
        [7_272_601, 7_272_602, 7_272_603, 7_272_604, 7_272_607],
        "every legacy shape that migrates is adopted, metadata-only included"
    );
    assert_eq!(
        channels(|s| matches!(s, Step::Hold(..))),
        [7_272_605, 7_272_606, 7_272_608],
        "unknown, corrupt and foreign-namespace files are held"
    );
    let at = |want: &Step| steps.iter().position(|s| s == want).expect("planned step");
    let head = sources::identity(&item(100));
    let inflight = at(&Step::Inflight(attempted, head));
    let marker = at(&Step::Marker(attempted));
    let placeholder = steps
        .iter()
        .position(
            |s| matches!(s, Step::Placeholder(c, owed) if c.get() == 7_272_607 && owed.len() == 1),
        )
        .expect("the retained notice is owed");
    let last_queue_step = steps
        .iter()
        .rposition(|s| matches!(s, Step::Adopt(_) | Step::Hold(..)))
        .unwrap();
    assert!(
        last_queue_step < inflight && inflight < marker && marker < placeholder,
        "queue, then inflight, then marker, then placeholder: {steps:?}"
    );
    for (file, before) in files {
        assert_eq!(
            std::fs::read(&file).unwrap(),
            before,
            "planning writes nothing"
        );
    }
}

#[test]
fn a_retained_only_mailbox_the_registry_dropped_is_readopted_with_its_notice() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(7_272_611);
        let (_, file) = owned(&registry, channel.get(), &[], true).await;
        let before = std::fs::read(&file).unwrap();
        assert_eq!(
            registry.remove_idle_entry(channel).await,
            MailboxPurgeOutcome::Removed
        );

        assert_eq!(readopt(&registry, &ctx(), NAMESPACE).await, [Ok(channel)]);
        let retained = registry
            .handle(channel)
            .v2_request(|slot| {
                let owner = slot.as_ref().ok_or("no owner")?;
                let notices: Vec<bool> = (owner.snapshot.retained_inputs.iter())
                    .map(|entry| entry.notice.enqueued)
                    .collect();
                Ok((owner.snapshot.items.len(), notices))
            })
            .await
            .unwrap();
        assert_eq!(
            retained,
            (0, vec![false]),
            "the retained input and its owed notice survive"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            before,
            "re-adoption rewrites nothing"
        );
        assert!(
            readopt(&registry, &ctx(), NAMESPACE).await.is_empty(),
            "a held owner is not adopted twice"
        );
    });
}

#[test]
fn queued_v2_inputs_hold_a_behind_queue_claim_but_retained_inputs_do_not() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        // Unpersisted, so the V1 disk check before the order check does not read the V2 file.
        let claim = |handle: ChannelMailboxHandle| async move {
            handle
                .try_start_turn_kinded_result(
                    Arc::new(CancelToken::new()),
                    poise::serenity_prelude::UserId::new(7),
                    MessageId::new(900),
                    ActiveTurnKind::UserOrAgent,
                    TurnAdmissionOrder::BehindQueue,
                    None,
                )
                .await
                .started
        };
        let (retained_only, _) = owned(&registry, 7_272_621, &[], true).await;
        assert!(
            claim(retained_only).await,
            "a retained input is not a barrier"
        );
        let (backlog, _) = owned(&registry, 7_272_622, &[200], false).await;
        assert!(
            !claim(backlog).await,
            "a queued V2 input is older work ahead of the claim"
        );
    });
}

#[test]
fn a_foreign_legacy_file_under_a_v2_owner_is_never_hydrated_or_requeued_into_v1() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(7_272_631);
        let (handle, file) = owned(&registry, channel.get(), &[100, 200], false).await;
        attempt_head(&handle).await;
        let ctx = ctx();
        save_channel_pending_dispatch_marker(
            &ctx.provider,
            &ctx.token_hash,
            channel,
            &intervention(100),
            None,
        )
        .unwrap();
        let marker = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "dispatch"))
            .unwrap();
        let marker_before = std::fs::read(&marker).unwrap();
        let foreign = serde_json::to_string(&vec![item(900)]).unwrap();
        std::fs::write(&file, &foreign).unwrap();

        let claim = handle
            .claim_queued_head(sources::identity(&item(900)), ctx.clone())
            .await;
        assert!(
            matches!(
                claim,
                crate::services::turn_orchestrator::QueuedHeadClaim::PersistenceFailed(_)
            ),
            "the claim refuses instead of hydrating the foreign file"
        );
        let requeued = handle
            .requeue_front_inner(intervention(500), ctx.clone(), None)
            .await;
        assert!(!requeued.enqueued && requeued.persistence_error.is_some());

        assert!(
            handle.snapshot().await.intervention_queue.is_empty(),
            "no V1 queue beside the owner"
        );
        assert_eq!(
            queued(&handle).await,
            (vec![100, 200], true),
            "order and attempted head kept"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            foreign,
            "the foreign bytes are not rewritten"
        );
        assert_eq!(
            std::fs::read(&marker).unwrap(),
            marker_before,
            "the marker is kept"
        );
    });
}

#[cfg(unix)]
#[test]
fn a_v2_checked_purge_discards_untried_items_only_while_the_capture_still_holds() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let (handle, file) = owned(&registry, 7_272_641, &[100, 200, 300], false).await;
        attempt_head(&handle).await;
        let capture = handle.try_capture_own_queue(ctx()).await.unwrap();
        assert!(
            capture.is_err(),
            "a V1 capture of a V2 queue is refused, not empty"
        );
        let fingerprint = || async {
            (handle
                .v2_request(|slot| slot.as_ref().ok_or("no owner")?.fingerprint())
                .await)
                .unwrap()
        };
        let expected = || async { ExpectedQueue::of(&handle.snapshot().await) };
        let purge = |expected: ExpectedQueue| handle.try_purge_queue_if_unchanged(ctx(), expected);

        let old = fingerprint().await;
        let v1_only = purge(expected().await).await.unwrap();
        assert_eq!(
            v1_only.queue_preserved(),
            Some(true),
            "a capture without the V2 snapshot purges nothing"
        );
        assert!(handle.enqueue(intervention(400), ctx()).await.enqueued);
        let stale = purge(expected().await.with_v2(old)).await.unwrap();
        assert!(stale.failed.is_some() && stale.receipt.is_empty());
        assert_eq!(
            stale.queue_preserved(),
            Some(true),
            "a refusal is never a purge"
        );
        assert_eq!(queued(&handle).await, (vec![100, 200, 300, 400], true));

        let current = fingerprint().await;
        let purged = purge(expected().await.with_v2(current)).await.unwrap();
        assert!(purged.failed.is_none());
        let receipt: Vec<u64> = purged.receipt.iter().map(|i| i.message_id.get()).collect();
        assert_eq!(receipt, [200, 300, 400], "only untried items are purged");
        assert_eq!(
            queued(&handle).await,
            (vec![100], true),
            "the attempted head stays in custody"
        );
        let bytes = std::fs::read_to_string(&file).unwrap();
        assert!(matches!(
            compat::inspect(&bytes),
            Ok(compat::SnapshotRead::ReadOnlyV2(_))
        ));
    });
}
