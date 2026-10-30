use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::provider::{CancelToken, ProviderKind};
#[cfg(unix)]
use crate::services::turn_orchestrator::checked_purge::ExpectedQueue;
use crate::services::turn_orchestrator::pending_queue_persistence::{
    pending_queue_item_to_intervention, save_channel_pending_dispatch_marker,
};
#[cfg(unix)]
use crate::services::turn_orchestrator::queued_head::compat;
use crate::services::turn_orchestrator::queued_head::{
    PersistedHeadInjection, canonical_item, mend::HeadAttempt, snapshot_json,
};
use crate::services::turn_orchestrator::{
    ActiveTurnKind, ChannelMailboxHandle, Intervention, TurnAdmissionOrder,
    registry_purge::MailboxPurgeOutcome,
};
use poise::serenity_prelude::MessageId;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

const NAMESPACE: &str = "projection";

fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "v2-projection", None)
}

/// Another token namespace of the same provider; its queue file does not exist.
fn other_ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "v2-projection-b", None)
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
                followers: Vec::new(),
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

    let steps = plan(&ctx, NAMESPACE).unwrap();
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

        assert_eq!(
            readopt(&registry, &ctx(), NAMESPACE).await,
            Ok(vec![Ok(channel)])
        );
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
            readopt(&registry, &ctx(), NAMESPACE).await == Ok(vec![]),
            "a held owner is not adopted twice"
        );
    });
}

#[test]
fn a_v2_owner_lets_persisted_claims_past_retained_inputs_only() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let claim = |handle: ChannelMailboxHandle, id: u64, order, persisted: bool| async move {
            handle
                .try_start_turn_kinded_result(
                    Arc::new(CancelToken::new()),
                    poise::serenity_prelude::UserId::new(7),
                    MessageId::new(id),
                    ActiveTurnKind::UserOrAgent,
                    order,
                    persisted.then(ctx),
                )
                .await
                .started
        };
        let (behind, now) = (
            TurnAdmissionOrder::BehindQueue,
            TurnAdmissionOrder::Immediate,
        );
        for persisted in [true, false] {
            let base = if persisted { 7_272_620 } else { 7_272_660 };
            let (retained_only, _) = owned(&registry, base + 1, &[], true).await;
            assert!(
                claim(retained_only, 900, behind, persisted).await,
                "retained is no barrier"
            );
            let (other, _) = owned(&registry, base + 2, &[200], false).await;
            assert!(
                !claim(other, 900, behind, persisted).await,
                "older queued work goes first"
            );
            let (attempted, _) = owned(&registry, base + 3, &[100], false).await;
            attempt_head(&attempted).await;
            assert!(
                !claim(attempted, 900, now, persisted).await,
                "an attempted head is never passed"
            );
        }
        // No owner hands its queued input to a turn yet, so the claimant's own input is withheld.
        for (channel, order) in [(7_272_624, behind), (7_272_625, now)] {
            let (sole, _) = owned(&registry, channel, &[900], false).await;
            assert!(
                !claim(sole.clone(), 900, order, true).await,
                "no turn beside its queued original"
            );
            assert_eq!(queued(&sole).await, (vec![900], false));
        }
        let unowned = registry.handle(ChannelId::new(7_272_626));
        assert!(
            claim(unowned, 900, behind, true).await,
            "without an owner the V1 rules apply"
        );
    });
}

/// Only the front the owner granted by exact ticket and original may start a turn; a claim naming
/// a source it holds never does, and a claim it never admitted keeps V1's order.
#[test]
fn a_v2_owner_starts_only_its_granted_front_and_never_a_held_source() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let (behind, now) = (
            TurnAdmissionOrder::BehindQueue,
            TurnAdmissionOrder::Immediate,
        );
        let blocks = |handle: &ChannelMailboxHandle, claims: Vec<(u64, TurnAdmissionOrder)>| {
            let handle = handle.clone();
            async move {
                handle
                    .v2_request(move |slot| {
                        let owner = slot.as_mut().ok_or("no owner")?;
                        Ok(claims
                            .into_iter()
                            .map(|(id, order)| owner.blocks_claim(id, None, order))
                            .collect::<Vec<_>>())
                    })
                    .await
                    .unwrap()
            }
        };
        let (metadata_only, _) = owned(&registry, 7_272_640, &[], false).await;
        assert_eq!(blocks(&metadata_only, vec![(900, behind)]).await, [false]);
        let (queued, _) = owned(&registry, 7_272_641, &[], false).await;
        queued
            .v2_admit_ticket(item(100), "front-100".into())
            .await
            .unwrap();
        assert_eq!(
            blocks(
                &queued,
                vec![(900, behind), (900, now), (100, behind), (100, now)]
            )
            .await,
            [true, false, true, true],
            "older queued work goes first, a process or recovery claim starts as in V1, and a \
             message ID alone never runs the owner's original"
        );
        let (retained, _) = owned(&registry, 7_272_644, &[], true).await;
        assert_eq!(
            blocks(&retained, vec![(300, now), (900, now)]).await,
            [true, false]
        );
        let spent = queued
            .v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("no owner")?;
                let front = owner.front().ok_or("no front")?;
                assert_eq!(front.0.as_deref(), Some("front-100"));
                let other = (Some("front-101".to_string()), front.1.clone());
                assert!(owner.grant_front(&other).is_err());
                owner.grant_front(&front)?;
                Ok([
                    owner.blocks_claim(101, None, behind),
                    owner.blocks_claim(100, None, behind),
                    owner.blocks_claim(100, Some(&front), behind),
                    owner.blocks_claim(100, Some(&front), behind),
                ])
            })
            .await
            .unwrap();
        assert_eq!(
            spent,
            [true, true, false, true],
            "a message ID never spends the grant; the exact front starts once"
        );
        let (attempted, _) = owned(&registry, 7_272_645, &[100], false).await;
        attempt_head(&attempted).await;
        assert_eq!(blocks(&attempted, vec![(900, now)]).await, [true]);
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

#[test]
fn a_requeue_under_a_v2_owner_is_refused_in_any_namespace_without_a_prior_claim() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(7_272_651);
        let (handle, file) = owned(&registry, channel.get(), &[100, 200], false).await;
        attempt_head(&handle).await;
        let before = std::fs::read(&file).unwrap();
        let (b, b_file) = (other_ctx(), {
            let other = other_ctx();
            pending_queue_file_path(&other.provider, &other.token_hash, channel).unwrap()
        });
        for persistence in [b, ctx()] {
            let requeued = handle
                .requeue_front_inner(intervention(500), persistence, None)
                .await;
            assert!(!requeued.enqueued && requeued.persistence_error.is_some());
            assert!(handle.snapshot().await.intervention_queue.is_empty());
            assert_eq!(queued(&handle).await, (vec![100, 200], true));
            assert_eq!(
                std::fs::read(&file).unwrap(),
                before,
                "the owner's file is untouched"
            );
            assert!(
                !b_file.exists(),
                "no queue file appears in the other namespace"
            );
        }
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
        let before = std::fs::read(&file).unwrap();
        let elsewhere = handle
            .try_purge_queue_if_unchanged(other_ctx(), expected().await.with_v2(current.clone()));
        let elsewhere = elsewhere.await.unwrap();
        assert!(elsewhere.failed.is_some() && elsewhere.receipt.is_empty());
        assert_eq!(
            elsewhere.queue_preserved(),
            Some(true),
            "another namespace purges nothing"
        );
        assert_eq!(queued(&handle).await, (vec![100, 200, 300, 400], true));
        assert_eq!(std::fs::read(&file).unwrap(), before);
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

/// A V1 capture refused under an owner picks no namespace and adopts no V1 record: once the owner
/// is gone, a third namespace still captures and the next writer reads the foreign record first.
#[cfg(unix)]
#[test]
fn a_v1_capture_refused_under_a_v2_owner_leaves_no_v1_namespace_or_record() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let third = QueuePersistenceContext::new(&ProviderKind::Claude, "v2-projection-c", None);
        for channel in [7_272_661, 7_272_662] {
            let (handle, _) = owned(&registry, channel, &[100], false).await;
            let refused = handle.try_capture_own_queue(other_ctx()).await.unwrap();
            assert!(refused.is_err(), "the owner's actor refuses a V1 capture");
            let released = handle.v2_request(|slot| Ok(slot.take().is_some())).await;
            assert_eq!(released, Ok(true));
            if channel == 7_272_661 {
                let captured = handle.try_capture_own_queue(third.clone()).await.unwrap();
                assert!(
                    captured.is_ok(),
                    "no namespace was picked: {:?}",
                    captured.err()
                );
                continue;
            }
            let b = other_ctx();
            let b_file =
                pending_queue_file_path(&b.provider, &b.token_hash, ChannelId::new(channel));
            let b_file = b_file.unwrap();
            std::fs::create_dir_all(b_file.parent().unwrap()).unwrap();
            std::fs::write(&b_file, envelope(vec![item(200), item(300)], false)).unwrap();
            let enqueued = handle.enqueue(intervention(400), b).await;
            assert!(enqueued.enqueued, "{:?}", enqueued.persistence_error);
            let written: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&b_file).unwrap()).unwrap();
            assert_eq!(
                written["terminal_identities"].as_array().map(Vec::len),
                Some(1),
                "the foreign record was read before the write and kept"
            );
        }
    });
}

#[test]
fn planning_reads_without_removing_stale_writer_files_and_reports_scan_errors() {
    let _root = TestRuntimeRootGuard::new();
    let file = seed(7_272_671, &serde_json::to_string(&vec![item(100)]).unwrap());
    let ctx = ctx();
    let channel = ChannelId::new(7_272_671);
    save_channel_pending_dispatch_marker(
        &ctx.provider,
        &ctx.token_hash,
        channel,
        &intervention(100),
        None,
    )
    .unwrap();
    let dir = file.parent().unwrap().to_path_buf();
    let stale = dir.join("7272671.json.tmp");
    std::fs::write(&stale, b"half written").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let listing = || {
        let mut entries: Vec<(PathBuf, Vec<u8>)> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .map(|path| (path.clone(), std::fs::read(path).unwrap()))
            .collect();
        entries.sort();
        entries
    };
    let before = listing();
    assert_eq!(
        plan(&ctx, NAMESPACE).unwrap().first(),
        Some(&Step::Adopt(channel))
    );
    assert_eq!(listing(), before, "the plan removes and rewrites nothing");

    let blocked = ChannelId::new(7_272_672);
    let other = other_ctx();
    let namespace_dir = pending_queue_file_path(&other.provider, &other.token_hash, blocked)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::create_dir_all(namespace_dir.parent().unwrap()).unwrap();
    std::fs::write(&namespace_dir, b"not a directory").unwrap();
    assert!(
        plan(&other, NAMESPACE).is_err(),
        "a scan error is not an empty plan"
    );
    run(async {
        let registry = ChannelMailboxRegistry::default();
        assert!(readopt(&registry, &other, NAMESPACE).await.is_err());
    });
    assert_eq!(std::fs::read(&namespace_dir).unwrap(), b"not a directory");
}

/// The V2 plan only reads; the V1 recovery entry removes stale writer temp files of its namespace
/// and nothing else, then restores the queue.
#[test]
fn v1_recovery_entry_removes_only_stale_temp_files_that_the_plan_left() {
    let _root = TestRuntimeRootGuard::new();
    let channel = ChannelId::new(7_272_681);
    let file = seed(
        channel.get(),
        &serde_json::to_string(&vec![item(100)]).unwrap(),
    );
    let ctx = ctx();
    save_channel_pending_dispatch_marker(
        &ctx.provider,
        &ctx.token_hash,
        channel,
        &intervention(100),
        None,
    )
    .unwrap();
    let dir = file.parent().unwrap().to_path_buf();
    let (stale, recent) = (
        dir.join("7272681.json.tmp"),
        dir.join("7272681.json.next.tmp"),
    );
    std::fs::write(&stale, b"half written").unwrap();
    std::fs::write(&recent, b"being written").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let file_at = |path: &PathBuf| std::fs::File::options().write(true).open(path).unwrap();
    file_at(&stale).set_modified(old).unwrap();
    let listing = || {
        let mut entries: Vec<(PathBuf, Vec<u8>)> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .map(|path| (path.clone(), std::fs::read(path).unwrap()))
            .collect();
        entries.sort();
        entries
    };
    let before = listing();
    assert!(plan(&ctx, NAMESPACE).is_ok());
    assert_eq!(listing(), before, "the plan removes nothing");

    let recovered =
        crate::services::turn_orchestrator::pending_queue_persistence::load_pending_queues_checked(
            &ctx.provider,
            &ctx.token_hash,
        );
    let mut expected = before;
    expected.retain(|(path, _)| *path != stale);
    assert_eq!(listing(), expected, "only the stale temp file is gone");
    let restored = recovered.queues.get(&channel).map(|queue| {
        queue
            .iter()
            .map(|input| input.message_id.get())
            .collect::<Vec<_>>()
    });
    assert_eq!(restored, Some(vec![100]), "the queue is restored");
}

/// A checked purge keeps every original a reserved unit carries, not only its first.
#[cfg(unix)]
#[test]
fn a_v2_checked_purge_keeps_every_member_of_a_reserved_unit() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let (handle, _) = owned(&registry, 7_272_642, &[100, 200, 300], false).await;
        handle
            .v2_request(|slot| {
                let owner = slot.as_mut().ok_or("no owner")?;
                let attempt = serde_json::from_value(json!({"nonce":"unit","provider":"claude",
                    "target":{"host":"tmux","owner":"projection-unit","root":"/test","execution_nonce":"e"},
                    "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}}))
                    .map_err(|e| e.to_string())?;
                let unit = |item: &PendingQueueItem| (item.message_id != 300).then(|| ("name".to_string(), item.text.clone()));
                owner.reserve_unit(attempt, unit).map(drop)
            })
            .await
            .unwrap();
        let current = (handle
            .v2_request(|slot| slot.as_ref().ok_or("no owner")?.fingerprint())
            .await)
            .unwrap();
        let expected = ExpectedQueue::of(&handle.snapshot().await).with_v2(current);
        let purged = handle
            .try_purge_queue_if_unchanged(ctx(), expected)
            .await
            .unwrap();
        let receipt: Vec<u64> = purged.receipt.iter().map(|i| i.message_id.get()).collect();
        assert_eq!(receipt, [300], "only what no unit holds is purged");
        assert_eq!(queued(&handle).await, (vec![100, 200], true));
    });
}

/// Ensure adopts a queue file once: the same file and namespace keep the owner and its grant, and
/// another file or namespace refuses.
#[test]
fn ensure_adopts_once_and_refuses_another_identity() {
    let _root = TestRuntimeRootGuard::new();
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let channel = 7_272_646;
        let handle = registry.handle(ChannelId::new(channel));
        let file = path(channel);
        let (first, second) = tokio::join!(
            handle.v2_ensure(file.clone(), NAMESPACE.into()),
            handle.v2_ensure(file.clone(), NAMESPACE.into())
        );
        assert_eq!((first, second), (Ok(()), Ok(())));
        handle
            .v2_request(|slot| {
                let owner = slot.as_mut().ok_or("no owner")?;
                owner
                    .enqueue_fixture(&item(100))
                    .map_err(|e| format!("{e:?}"))?;
                let front = owner.front().ok_or("no front")?;
                owner.grant_front(&front)
            })
            .await
            .unwrap();
        handle
            .v2_ensure(file.clone(), NAMESPACE.into())
            .await
            .unwrap();
        let claimed = handle.v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let front = owner.front().ok_or("no front")?;
            Ok(owner.claim_front(&front))
        });
        assert!(claimed.await.unwrap(), "the same owner, its grant kept");
        let elsewhere = file.with_file_name("7272646-other.json");
        assert!(handle.v2_ensure(elsewhere, NAMESPACE.into()).await.is_err());
        assert!(handle.v2_ensure(file, "other".into()).await.is_err());
    });
}
