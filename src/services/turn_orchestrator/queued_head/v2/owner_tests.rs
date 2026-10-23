use super::super::super::{ChannelMailboxHandle, ChannelMailboxRegistry, QueuePersistenceContext};
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn item(id: u64) -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":7272,
        "source_message_ids":[id,id+10000],"queued_generation":5,"text":"same original\nwith details"})).unwrap()
}

fn attempt(id: u64, owner: &str) -> AttemptIdentity {
    AttemptIdentity {
        nonce: format!("nonce-{id}"),
        provider: "codex".into(),
        target: TargetIdentity {
            host: "tmux".into(),
            owner: owner.into(),
            root: "/test".into(),
            execution_nonce: "execution".into(),
        },
        source: AttemptSource::Existing {
            session_id: "native-session".into(),
            transcript: "/test/rollout".into(),
            file: [1, 2],
            offset: 20,
        },
    }
}

fn payload() -> PreparedPayload {
    PreparedPayload {
        frame: "exact prepared frame".into(),
        digest: "digest".into(),
        attribution: json!({"author":7}),
    }
}

fn path(channel: u64) -> PathBuf {
    super::super::pending_queue_file_path(
        &ProviderKind::Claude,
        "v2-owner",
        ChannelId::new(channel),
    )
    .unwrap()
}

fn owner(channel: u64) -> Owner {
    Owner::adopt(path(channel), format!("scope-{channel}")).unwrap()
}

async fn prepare(handle: &ChannelMailboxHandle, id: u64, target: &str) -> AttemptRef {
    let target = target.to_string();
    handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            assert!(owner.enqueue(item(id))?);
            let key = owner.reserve(attempt(id, &target), payload())?;
            assert_eq!(
                key.identity.primary, id,
                "retained original must not become the next executable head"
            );
            Ok(key)
        })
        .await
        .unwrap()
}

async fn snapshot(handle: &ChannelMailboxHandle) -> Value {
    handle
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap()
}

#[tokio::test]
async fn v2_mailbox_retains_without_releasing_live_writer_and_late_settles_exactly() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(727201));
    handle
        .v2_adopt(path(727201), "scope-727201".into())
        .await
        .unwrap();
    let a = prepare(&handle, 100, "pane-concurrency").await;
    let effects = Arc::new(AtomicUsize::new(0));
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (exit, exit_rx) = std::sync::mpsc::channel();
    let task = tokio::spawn({
        let h = handle.clone();
        let effects = effects.clone();
        async move {
            h.v2_effect(1000, move |frame| {
                assert_eq!(frame, "exact prepared frame");
                effects.fetch_add(1, Ordering::SeqCst);
                entered.send(()).unwrap();
                exit_rx.recv().unwrap();
                Ok(true)
            })
            .await
        }
    });
    entered_rx.await.unwrap();
    task.abort(); // The detached owner and blocking writer must still hold the physical lease.
    handle
        .v2_request(|slot| slot.as_mut().unwrap().tick(30_999))
        .await
        .unwrap();
    assert!(
        snapshot(&handle).await["retained_inputs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    handle
        .v2_request(|slot| slot.as_mut().unwrap().tick(31_000))
        .await
        .unwrap();
    let retained = snapshot(&handle).await;
    assert_eq!(
        retained["retained_inputs"].as_array().unwrap().len(),
        1,
        "deadline must detach exactly one retained input"
    );
    assert_eq!(
        retained["retained_inputs"][0]["input"]["original"]["text"],
        item(100).text,
        "deadline retains A original and releases only its logical head"
    );
    let b = prepare(&handle, 200, "pane-concurrency").await;
    let before = snapshot(&handle).await;
    assert!(
        handle.v2_effect(31_001, |_| Ok(true)).await.is_err(),
        "logical expiry must not release a live physical writer"
    );
    assert_eq!(
        snapshot(&handle).await,
        before,
        "blocked writer must not change B phase or deadline"
    );
    let mut wrong = a.clone();
    wrong.attempt.nonce.push('x');
    assert!(
        handle
            .v2_settle_exact(serde_json::to_value(wrong).unwrap(), true, 31_002)
            .await
            .is_err(),
        "nonce mismatch must never settle any exact attempt"
    );
    exit.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while snapshot(&handle).await["terminal_identities"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let after = snapshot(&handle).await;
    assert_eq!(
        after["head_injection"], before["head_injection"],
        "late A never settles current B"
    );
    assert_eq!(after["items"], before["items"]);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert!(!handle.v2_effect(32_000, |_| Ok(false)).await.unwrap());
    handle
        .v2_request(|slot| slot.as_mut().unwrap().tick(62_000))
        .await
        .unwrap();
    let _c = prepare(&handle, 300, "pane-concurrency").await;
    assert!(handle.v2_effect(63_000, |_| Ok(true)).await.unwrap());
    assert_eq!(
        snapshot(&handle).await["retained_inputs"][0]["input"]["identity"],
        serde_json::to_value(&b.identity).unwrap()
    );
}

#[test]
fn v2_commit_readback_requires_full_candidate_and_sync_before_permit() {
    let _root = TestRuntimeRootGuard::new();
    for (index, fault) in [
        Fault::Before,
        Fault::AfterRename,
        Fault::Sync,
        Fault::Read,
        Fault::Third,
    ]
    .into_iter()
    .enumerate()
    {
        let mut owner = owner(727210 + index as u64);
        owner.enqueue(item(100)).unwrap();
        let key = owner
            .reserve(attempt(100, &format!("pane-commit-{index}")), payload())
            .unwrap();
        owner.fault = fault;
        let permit = owner.arm(&key, 1000);
        if fault == Fault::AfterRename {
            assert!(
                permit.is_ok(),
                "exact candidate readback recovers post-rename error"
            );
            drop(permit);
        } else {
            assert!(
                permit.is_err(),
                "unconfirmed publication must issue zero permits"
            );
            assert!(
                owner
                    .snapshot
                    .head_injection
                    .as_ref()
                    .unwrap()
                    .confirmation_deadline_ms
                    .is_none()
            );
            if fault != Fault::Before {
                assert!(
                    owner.tick(90_000).is_err(),
                    "unknown commit blocks further advance"
                );
                if fault == Fault::Third {
                    assert_eq!(std::fs::read_to_string(&owner.path).unwrap(), "third state");
                    let candidate = &owner.pending.as_ref().unwrap().0;
                    std::fs::write(&owner.path, candidate).unwrap();
                }
                owner.fault = Fault::None;
                owner.recover().unwrap();
                assert_eq!(
                    owner
                        .snapshot
                        .head_injection
                        .as_ref()
                        .unwrap()
                        .confirmation_deadline_ms,
                    Some(31_000)
                );
                assert!(
                    owner.arm(&key, 100_000).is_err(),
                    "recovery never reissues an effect permit"
                );
            }
        }
    }
    let mut owner = owner(727219);
    owner.enqueue(item(100)).unwrap();
    let mut changed: Value = serde_json::from_str(owner.bytes.as_ref().unwrap()).unwrap();
    changed["items"][0]["text"] = json!("externally different original");
    std::fs::write(&owner.path, changed.to_string()).unwrap();
    assert!(
        owner.enqueue(item(200)).is_err(),
        "comparison must include original items, not metadata alone"
    );
}

#[test]
fn v2_restart_adopts_reserved_and_unknown_without_reset_or_resend() {
    let _root = TestRuntimeRootGuard::new();
    for (index, phase) in [
        HeadPhase::Reserved,
        HeadPhase::MayHaveSent,
        HeadPhase::Unknown,
    ]
    .into_iter()
    .enumerate()
    {
        let channel = 727230 + index as u64;
        let mut first = owner(channel);
        first.enqueue(item(100)).unwrap();
        let key = first
            .reserve(attempt(100, "pane-restart"), payload())
            .unwrap();
        if phase != HeadPhase::Reserved {
            drop(first.arm(&key, 1000).unwrap());
            if phase == HeadPhase::Unknown {
                first.settle(&key, false, 1001).unwrap();
            }
        }
        drop(first);
        let mut restored = owner(channel);
        if phase == HeadPhase::Reserved {
            assert!(restored.snapshot.head_injection.is_none());
            assert_eq!(restored.snapshot.items.len(), 1);
        } else {
            assert_eq!(
                restored
                    .snapshot
                    .head_injection
                    .as_ref()
                    .unwrap()
                    .confirmation_deadline_ms,
                Some(31_000),
                "restart preserves effect deadline"
            );
            assert!(
                restored.arm(&key, 30_000).is_err(),
                "restart never retries an uncertain attempt"
            );
            restored.tick(30_000).unwrap();
            assert!(restored.snapshot.retained_inputs.is_empty());
            restored.tick(31_000).unwrap();
            drop(restored);
            let context = QueuePersistenceContext::new(&ProviderKind::Claude, "v2-owner", None);
            let catalog =
                super::super::super::pending_queue_persistence::queued_recovery::read_catalog(
                    &context.provider,
                    &context.token_hash,
                );
            assert!(
                catalog.snapshots.contains_key(&ChannelId::new(channel)),
                "metadata-only catalog keeps retained channels"
            );
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let registry = ChannelMailboxRegistry::default();
                    let (&catalog_channel, _) = catalog
                        .snapshots
                        .iter()
                        .find(|(key, _)| key.get() == channel)
                        .unwrap();
                    let handle = registry.handle(catalog_channel);
                    handle
                        .v2_adopt(path(catalog_channel.get()), format!("scope-{channel}"))
                        .await
                        .unwrap();
                    let restored = snapshot(&handle).await;
                    assert!(restored["items"].as_array().unwrap().is_empty());
                    assert_eq!(
                        restored["retained_inputs"].as_array().unwrap().len(),
                        1,
                        "catalog metadata-only entry must reach actor adoption"
                    );
                });
        }
    }
}

#[test]
fn v2_source_fences_ignore_generation_and_terminal_age_begins_at_settlement() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(727240);
    owner.enqueue(item(100)).unwrap();
    let key = owner
        .reserve(attempt(100, "pane-source"), payload())
        .unwrap();
    drop(owner.arm(&key, 1000).unwrap());
    owner.tick(31_000).unwrap();
    let mut replay = item(10100);
    replay.queued_generation = 900;
    assert!(
        !owner.enqueue(replay.clone()).unwrap(),
        "merged secondary source is fenced across generations"
    );
    owner.tick(9_000_000_000).unwrap();
    assert_eq!(
        owner.snapshot.retained_inputs.len(),
        1,
        "unresolved original has no TTL or count eviction"
    );
    owner.settle(&key, true, 9_000_000_000).unwrap();
    owner.tick(9_086_399_999).unwrap();
    assert!(
        !owner.enqueue(replay.clone()).unwrap(),
        "terminal survives a full day from settlement"
    );
    let mut other = Owner::adopt(path(727241), "other-namespace".into()).unwrap();
    assert!(
        other.enqueue(replay.clone()).unwrap(),
        "another namespace is independent"
    );
    owner.tick(9_086_400_000).unwrap();
    assert!(owner.enqueue(replay).unwrap());
}

#[test]
fn v2_parked_and_slot_projection_preserve_uncertainty_and_late_identity() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(727250);
    sources::migrate_parked(&mut owner, item(100), false).unwrap();
    sources::migrate_parked(&mut owner, item(200), true).unwrap();
    assert_eq!(owner.snapshot.retained_inputs.len(), 1);
    assert_eq!(owner.snapshot.items.len(), 1);
    sources::migrate_parked(&mut owner, item(100), false).unwrap();
    assert_eq!(
        owner.snapshot.retained_inputs.len(),
        1,
        "Parked migration is idempotent"
    );
    let mut owner = Owner::adopt(path(727251), "scope-727251".into()).unwrap();
    for n in 0..10 {
        let id = 1000 + n;
        let nonce = format!("nonce-{id}");
        assert!(
            crate::services::tui_prompt_dedupe::register_injected_steer(
                "codex",
                "pane-slots",
                &nonce,
                "turn",
                Some(id)
            ),
            "nine Unknown inputs must release pending capacity"
        );
        owner.enqueue(item(id)).unwrap();
        let key = owner.reserve(attempt(id, "pane-slots"), payload()).unwrap();
        drop(owner.arm(&key, 1000).unwrap());
        owner.tick(31_000).unwrap();
    }
    assert_eq!(owner.snapshot.retained_inputs.len(), 10);
    let oldest = owner.snapshot.retained_inputs[0]
        .input
        .attempt
        .as_ref()
        .unwrap();
    assert_eq!(oldest.nonce, "nonce-1000");
}

#[test]
fn v2_retained_snapshot_cost_is_measured_without_compacting_originals() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(727260);
    owner.enqueue(item(100)).unwrap();
    let key = owner.reserve(attempt(100, "pane-size"), payload()).unwrap();
    drop(owner.arm(&key, 0).unwrap());
    owner.tick(30_000).unwrap();
    let template = owner.snapshot.retained_inputs[0].clone();
    for count in [0, 32, 256, 1024] {
        let mut next = owner.snapshot.clone();
        next.retained_inputs = vec![template.clone(); count];
        owner.fault = Fault::AfterRename;
        owner.commit(next).unwrap();
        let bytes = std::fs::metadata(&owner.path).unwrap().len();
        println!(
            "RETAINED_COST R={count} snapshot_bytes={bytes} candidate_full_compare_bytes={bytes} candidate_readback_bytes={bytes}"
        );
        assert_eq!(owner.snapshot.retained_inputs.len(), count);
    }
    for id in 20_000..20_030 {
        assert!(
            owner.enqueue(item(id)).unwrap(),
            "retained metadata must not consume executable queue capacity"
        );
    }
    assert!(owner.enqueue(item(20_030)).is_err());
    assert_eq!(owner.snapshot.retained_inputs.len(), 1024);
}

#[test]
fn v2_late_success_and_failure_never_settle_the_current_head() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(727265);
    owner.enqueue(item(100)).unwrap();
    let a = owner.reserve(attempt(100, "pane-late"), payload()).unwrap();
    drop(owner.arm(&a, 1000).unwrap());
    owner.tick(31_000).unwrap();
    owner.enqueue(item(200)).unwrap();
    let b = owner.reserve(attempt(200, "pane-late"), payload()).unwrap();
    drop(owner.arm(&b, 32_000).unwrap());
    let before = owner.json().unwrap();
    owner.settle(&a, false, 32_000).unwrap();
    assert_eq!(
        owner.json().unwrap()["head_injection"],
        before["head_injection"],
        "late failure for A never changes active B"
    );
    owner.settle(&a, true, 32_001).unwrap();
    assert_eq!(
        owner.json().unwrap()["head_injection"],
        before["head_injection"],
        "late success for A never changes active B"
    );
    assert_eq!(owner.json().unwrap()["items"], before["items"]);
}

#[test]
fn v2_exact_attempt_checks_every_source_and_target_field() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(727266);
    owner.enqueue(item(100)).unwrap();
    let key = owner
        .reserve(attempt(100, "pane-fields"), payload())
        .unwrap();
    owner.tick(999_000).unwrap();
    assert!(
        owner
            .snapshot
            .head_injection
            .as_ref()
            .unwrap()
            .confirmation_deadline_ms
            .is_none(),
        "preparation consumes none of the effect confirmation window"
    );
    let permit = owner.arm(&key, 1_000_000).unwrap();
    let mut wrong = key.clone();
    wrong.attempt.nonce.push('x');
    assert!(
        permit.consume(&wrong).is_err(),
        "a permit cannot execute another attempt"
    );
    assert!(
        owner.arm(&key, 1_000_001).is_err(),
        "a consumed permit is never reissued even with a free writer"
    );
    let raw = serde_json::to_value(&key).unwrap();
    for pointer in [
        "/identity/primary",
        "/attempt/nonce",
        "/attempt/provider",
        "/attempt/target/host",
        "/attempt/target/owner",
        "/attempt/target/root",
        "/attempt/target/execution_nonce",
        "/attempt/source/session_id",
        "/attempt/source/transcript",
        "/attempt/source/file/0",
        "/attempt/source/offset",
    ] {
        let mut changed = raw.clone();
        let value = changed.pointer_mut(pointer).unwrap();
        *value = if let Some(n) = value.as_u64() {
            json!(n + 1)
        } else {
            json!("different")
        };
        let changed = serde_json::from_value(changed).unwrap();
        assert!(
            owner.settle(&changed, true, 1_000_001).is_err(),
            "changed exact attempt field must be rejected: {pointer}"
        );
    }
    owner.settle(&key, true, 1_000_002).unwrap();
}

#[test]
fn v2_writer_lease_survives_owner_adoption() {
    let _root = TestRuntimeRootGuard::new();
    let mut first = owner(727267);
    first.enqueue(item(100)).unwrap();
    let a = first
        .reserve(attempt(100, "pane-adoption"), payload())
        .unwrap();
    let permit = first.arm(&a, 1000).unwrap();
    first.tick(31_000).unwrap();
    drop(first);
    let mut restored = owner(727267);
    restored.enqueue(item(200)).unwrap();
    let b = restored
        .reserve(attempt(200, "pane-adoption"), payload())
        .unwrap();
    assert!(
        restored.arm(&b, 32_000).is_err(),
        "adoption cannot release a still-live physical writer"
    );
    drop(permit.consume(&a).unwrap());
    assert!(restored.arm(&b, 32_001).is_ok());
}

#[tokio::test]
async fn v2_unsupported_host_veto_precedes_may_have_sent_and_effect() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(727269));
    handle
        .v2_adopt(path(727269), "scope-727269".into())
        .await
        .unwrap();
    handle
        .v2_request(|slot| {
            let owner = slot.as_mut().unwrap();
            owner.enqueue(item(100))?;
            let mut key = attempt(100, "herdr-not-connected");
            key.target.host = "herdr".into();
            owner.reserve(key, payload())?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        handle
            .v2_effect(1000, |_| panic!("unsupported adapter cannot run"))
            .await
            .is_err()
    );
    let state = snapshot(&handle).await;
    assert_eq!(state["head_injection"]["phase"], "reserved");
    assert!(
        state["head_injection"]["confirmation_deadline_ms"].is_null(),
        "unsupported host veto consumes no effect window"
    );
}

#[test]
fn v2_legacy_restore_uses_positive_reserved_proof_and_retains_unknown_without_ledger() {
    let _root = TestRuntimeRootGuard::new();
    for (n, phase) in [
        HeadPhase::Reserved,
        HeadPhase::MayHaveSent,
        HeadPhase::Unknown,
    ]
    .into_iter()
    .enumerate()
    {
        let channel = 727280 + n as u64;
        let mut original = item(100);
        original.created_at_wall_time_ms = Some(12345);
        let intervention =
            super::super::super::pending_queue_persistence::pending_queue_item_to_intervention(
                original.clone(),
                std::time::SystemTime::now(),
                std::time::Instant::now(),
            );
        let canonical = super::super::canonical_item(&intervention, ChannelId::new(7272));
        let meta = super::super::SnapshotMeta {
            head_injection: Some(super::super::PersistedHeadInjection {
                identity: sources::identity(&original),
                original: canonical,
                phase,
                attempt: None,
            }),
            ..Default::default()
        };
        let raw = super::super::snapshot_json(vec![original], Some(&meta)).unwrap();
        let file = path(channel);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, raw).unwrap();
        let mut restored = owner(channel);
        restored.tick(0).unwrap();
        if phase == HeadPhase::Reserved {
            assert_eq!(restored.snapshot.items.len(), 1);
            assert!(restored.snapshot.head_injection.is_none());
        } else {
            assert!(
                restored.snapshot.items.is_empty(),
                "ledger absence never proves an uncertain legacy attempt NotSent"
            );
            assert_eq!(restored.snapshot.retained_inputs.len(), 1);
            assert_eq!(
                restored.snapshot.retained_inputs[0].input.original.text,
                item(100).text
            );
        }
    }
}

#[test]
fn v2_adoption_reapplies_slots_after_crash_between_commit_and_projection() {
    let _root = TestRuntimeRootGuard::new();
    let mut first = owner(727268);
    for n in 0..8 {
        assert!(crate::services::tui_prompt_dedupe::register_injected_steer(
            "codex",
            "pane-projection",
            &format!("nonce-{}", 2000 + n),
            "turn",
            Some(2000 + n)
        ));
    }
    first.fault = Fault::BeforeProjection;
    for n in 0..8 {
        first.enqueue(item(2000 + n)).unwrap();
        let key = first
            .reserve(attempt(2000 + n, "pane-projection"), payload())
            .unwrap();
        drop(first.arm(&key, 1000).unwrap());
        first.tick(31_000).unwrap();
    }
    assert!(
        !crate::services::tui_prompt_dedupe::register_injected_steer(
            "codex",
            "pane-projection",
            "next",
            "turn",
            None
        )
    );
    drop(first);
    let restored = owner(727268);
    assert_eq!(restored.snapshot.retained_inputs.len(), 8);
    assert!(
        crate::services::tui_prompt_dedupe::register_injected_steer(
            "codex",
            "pane-projection",
            "next",
            "turn",
            None
        ),
        "adoption must replay committed retained slot projections"
    );
}

#[path = "owner_tests/pg_tests.rs"]
mod pg_tests;
