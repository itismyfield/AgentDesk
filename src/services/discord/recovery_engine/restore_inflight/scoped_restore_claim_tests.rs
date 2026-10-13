//! A scoped install claims through one actor step, so its queue, debt, actor and record land
//! together; what the record owes moves only by an exact reservation and the driver's confirm.

use super::b3a1_tests::{confirm_ready, context, drive, fixture, queued, row};
use super::b3a2_tests::{BARRIER, barrier};
use super::*;
use crate::services::turn_orchestrator::RecoveryKickoffResult;
use crate::services::turn_orchestrator::installed_actor_handoff::{
    self as handoff, Installation, OwedRecord, PendingDriver, PendingFeedback,
};
use std::sync::atomic::{AtomicU64, Ordering};

/// Everything a refused or cancelled step leaves as it was: the canonical queue file's bytes, the
/// queue, the slot, the activation marker and the actor's record.
#[derive(Debug, PartialEq)]
struct Seen {
    bytes: Option<Vec<u8>>,
    queue: Vec<u64>,
    slot: Option<usize>,
    started: bool,
    record: Option<OwedRecord>,
}

fn persistence(shared: &Arc<SharedData>, channel: ChannelId) -> QueuePersistenceContext {
    queue_persistence_context(shared, &ProviderKind::Claude, channel)
}

async fn seen(shared: &Arc<SharedData>, channel: ChannelId) -> Seen {
    let snapshot = shared.mailbox(channel).snapshot().await;
    let queue = snapshot.intervention_queue.iter();
    Seen {
        bytes: std::fs::read(handoff::queue_file(&persistence(shared, channel), channel)).ok(),
        queue: queue.map(|item| item.message_id.get()).collect(),
        slot: (snapshot.cancel_token.as_ref()).map(|token| Arc::as_ptr(token) as usize),
        started: snapshot.recovery_started_at.is_some(),
        record: shared.mailbox(channel).owed_record().await,
    }
}

/// A canonical row with its queued original, installed at generation 1.
async fn installed(shared: &Arc<SharedData>, channel: u64) -> InstalledInflight {
    let state = row(channel, Some("b3b1b2"));
    inflight::save_inflight_state_create_new(&state).unwrap();
    queued(shared, &state).await;
    let context = context(channel, 1, Arc::new(AtomicU64::new(1)));
    let http = Arc::new(serenity::Http::new("test"));
    let installed = install_inflight_scoped(&http, shared, &ProviderKind::Claude, &context);
    installed.await.unwrap()
}

/// The actor's own checks alone, with a row check that reads nothing.
async fn unchecked(
    shared: &Arc<SharedData>,
    proof: &Installation,
    intent: Intent,
) -> HandoffOutcome {
    let pass = || Ok(Box::new(()) as Box<dyn Send>);
    (shared.mailbox(proof.channel))
        .handoff_installed(proof.clone(), intent, pass)
        .await
}

/// Puts `token` in the slot under `proof`'s owner and message, as a same-nonce successor would.
async fn occupy(shared: &Arc<SharedData>, proof: &Installation, token: Arc<CancelToken>) {
    let kind = crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent;
    let mailbox = shared.mailbox(proof.channel);
    mailbox
        .restore_active_turn_kinded(token, proof.owner, proof.message, kind)
        .await;
}

/// An install cancelled before its claim, or while its claim's save is being settled, either
/// changed nothing or left the actor, its record and the debt together.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3b_install_cancelled_around_its_claim_commit_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, shared) = fixture().await;
    for (n, case) in ["before claim", "not committed", "committed"]
        .into_iter()
        .enumerate()
    {
        let state = row(5_340_431 + n as u64, Some("cancel"));
        inflight::save_inflight_state_create_new(&state).unwrap();
        queued(&shared, &state).await;
        let channel = ChannelId::new(state.channel_id);
        let before = seen(&shared, channel).await;
        let (entered, inside) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel::<()>();
        if case != "before claim" {
            let (path, old) = (
                handoff::queue_file(&persistence(&shared, channel), channel),
                before.bytes.clone().unwrap(),
            );
            handoff::fail_after_publish(channel, move || {
                entered.send(()).unwrap();
                held.recv().unwrap();
                if case == "not committed" {
                    std::fs::write(path, old).unwrap();
                }
            });
        }
        let context = context(state.channel_id, 1, Arc::new(AtomicU64::new(1)));
        let gate = barrier("install-admission");
        let target = shared.clone();
        let install = tokio::spawn(BARRIER.scope(gate.clone(), async move {
            let http = Arc::new(serenity::Http::new("test"));
            let installed =
                install_inflight_scoped(&http, &target, &ProviderKind::Claude, &context);
            installed.await.map(|_| ())
        }));
        gate.entered.notified().await;
        if case != "before claim" {
            gate.release.notify_one();
            let waited = tokio::task::spawn_blocking(move || inside.recv());
            waited.await.unwrap().unwrap();
        }
        install.abort();
        assert!(install.await.unwrap_err().is_cancelled(), "{case}");
        let _ = release.send(());
        let after = seen(&shared, channel).await;
        if case != "committed" {
            assert_eq!(after, before, "{case}");
            continue;
        }
        let record = after.record.expect("the claimed actor keeps its record");
        let debts = handoff::recorded_queue(&persistence(&shared, channel), channel);
        let debts = debts.unwrap().expect("queue file").debts;
        assert_eq!(
            (after.slot, &after.queue),
            (Some(record.actor), &Vec::new())
        );
        assert_eq!(record.exits, debts);
        assert_eq!(
            (record.feedback, record.driver, after.started),
            (PendingFeedback::Owed, PendingDriver::NotScheduled, false)
        );
    }
    pool.close().await;
    db.drop().await;
}

/// The actor refuses every inexact proof for every intent before its record moves: another Arc in
/// the slot, another birth, episode, owner or message, a stale generation, a changed row.
#[tokio::test]
async fn s3b_handoff_refuses_every_inexact_proof_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, shared) = fixture().await;
    let installed = installed(&shared, 5_340_441).await;
    let (proof, state) = (
        installed.actor.clone().unwrap(),
        installed.state.clone().unwrap(),
    );
    let channel = proof.channel;
    assert_eq!(
        drive(&shared, &installed).await,
        RecoveryKickoffResult::AlreadyActiveSameEpisode
    );
    let held = seen(&shared, channel).await;
    assert_eq!(
        held.record.as_ref().unwrap().driver,
        PendingDriver::Reserved
    );
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        state.channel_id,
    );
    let row_bytes = std::fs::read(&path).unwrap();
    type Tamper = (&'static str, fn(&mut Installation));
    let tampered: [Tamper; 5] = [
        ("birth", |p| p.birth.push_str("-reborn")),
        ("episode", |p| p.episode.push_str("-next")),
        ("owner", |p| p.owner = UserId::new(8)),
        ("message", |p| p.message = MessageId::new(72)),
        ("generation", |p| p.generation = 2),
    ];
    for intent in [Intent::Feedback, Intent::Driver, Intent::Ready] {
        for (case, tamper) in tampered {
            let mut other = proof.clone();
            tamper(&mut other);
            let outcome = unchecked(&shared, &other, intent).await;
            assert!(
                matches!(outcome, HandoffOutcome::Refused(_)),
                "{case} {intent:?}"
            );
            assert_eq!(seen(&shared, channel).await, held, "{case} {intent:?}");
        }
        proof.current.store(2, Ordering::Release);
        let stale = checked_handoff(
            &shared,
            &ProviderKind::Claude,
            &state,
            proof.clone(),
            intent,
        );
        assert!(
            matches!(stale.await, HandoffOutcome::Refused(_)),
            "stale {intent:?}"
        );
        proof.current.store(1, Ordering::Release);
        assert_eq!(seen(&shared, channel).await, held, "stale {intent:?}");

        let successor = Arc::new(CancelToken::from_persisted_turn_nonce(
            state.turn_nonce.clone(),
        ));
        occupy(&shared, &proof, successor.clone()).await;
        let swapped = seen(&shared, channel).await;
        assert_eq!(swapped.slot, Some(Arc::as_ptr(&successor) as usize));
        let foreign = checked_handoff(
            &shared,
            &ProviderKind::Claude,
            &state,
            proof.clone(),
            intent,
        );
        assert!(
            matches!(foreign.await, HandoffOutcome::Refused(_)),
            "actor B {intent:?}"
        );
        assert_eq!(seen(&shared, channel).await, swapped, "actor B {intent:?}");
        occupy(&shared, &proof, proof.token.clone()).await;

        let mut successor_row = state.clone();
        successor_row.started_at.push_str("-successor");
        std::fs::write(&path, serde_json::to_vec(&successor_row).unwrap()).unwrap();
        let changed = checked_handoff(
            &shared,
            &ProviderKind::Claude,
            &state,
            proof.clone(),
            intent,
        );
        assert!(
            matches!(changed.await, HandoffOutcome::Refused(_)),
            "row {intent:?}"
        );
        std::fs::write(&path, &row_bytes).unwrap();
        assert_eq!(seen(&shared, channel).await, held, "row {intent:?}");
    }
    let exact = checked_handoff(
        &shared,
        &ProviderKind::Claude,
        &state,
        proof,
        Intent::Feedback,
    );
    let HandoffOutcome::Reserved(owed) = exact.await else {
        panic!("the exact actor reserves its feedback");
    };
    let record = seen(&shared, channel).await.record.unwrap();
    let owed = owed.iter().map(|exit| {
        let message = exit.event.intervention.message_id.get();
        (exit.event_id.clone(), exit.episode.clone(), message)
    });
    assert_eq!(owed.collect::<Vec<_>>(), record.exits);
    assert_eq!(record.feedback, PendingFeedback::Reserved);
    pool.close().await;
    db.drop().await;
}

/// A reserved driver activates nothing and can be reserved again or re-installed; only the exact
/// actor's ready confirm moves it to ready, after which a driver is no longer owed.
#[tokio::test]
async fn s3b_driver_waits_for_its_exact_ready_confirm_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, shared) = fixture().await;
    let installed = installed(&shared, 5_340_451).await;
    let proof = installed.actor.clone().unwrap();
    let channel = proof.channel;
    assert!(installed.activated);
    for _ in 0..2 {
        assert_eq!(
            drive(&shared, &installed).await,
            RecoveryKickoffResult::AlreadyActiveSameEpisode
        );
        let reserved = seen(&shared, channel).await;
        assert_eq!(
            reserved.record.as_ref().unwrap().driver,
            PendingDriver::Reserved
        );
        assert!(!reserved.started, "a reservation activates nothing");
    }
    let held = seen(&shared, channel).await;
    let http = Arc::new(serenity::Http::new("test"));
    let again = context(channel.get(), 1, proof.current.clone());
    let again = install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &again).await;
    assert!(again.unwrap().activated, "a reserved driver is still owed");
    assert_eq!(seen(&shared, channel).await, held);

    let foreign = Installation {
        owner: UserId::new(8),
        ..proof.clone()
    };
    assert!(matches!(
        unchecked(&shared, &foreign, Intent::Ready).await,
        HandoffOutcome::Refused(_)
    ));
    assert_eq!(seen(&shared, channel).await, held);
    assert!(matches!(
        confirm_ready(&shared, &installed).await,
        HandoffOutcome::Ready
    ));
    let ready = seen(&shared, channel).await;
    assert_eq!(ready.record.as_ref().unwrap().driver, PendingDriver::Ready);
    assert!(ready.started);
    assert_eq!(
        drive(&shared, &installed).await,
        RecoveryKickoffResult::Unavailable
    );
    pool.close().await;
    db.drop().await;
}

/// The real install removes the queued original and writes the remaining queue and its debt in
/// one versioned snapshot whose debt is the record's.
#[tokio::test]
async fn s3b_install_writes_queue_and_debt_in_one_snapshot_pg() {
    use crate::services::turn_orchestrator::{Intervention, InterventionMode};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, shared) = fixture().await;
    let state = row(5_340_461, Some("debt"));
    inflight::save_inflight_state_create_new(&state).unwrap();
    queued(&shared, &state).await;
    let channel = ChannelId::new(state.channel_id);
    let id = MessageId::new(75);
    let later = Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: id,
        queued_generation: 1,
        source_message_ids: vec![id],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "queued later".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    };
    let mailbox = shared.mailbox(channel);
    let merged = mailbox.merge_restored_queue_items(vec![later], persistence(&shared, channel));
    assert_eq!(merged.await.absorbed, 1);
    let context = context(state.channel_id, 1, Arc::new(AtomicU64::new(1)));
    let http = Arc::new(serenity::Http::new("test"));
    let installed = install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &context);
    let installed = installed.await.unwrap();
    assert!(installed.activated);
    let proof = installed.actor.unwrap();
    let file = handoff::recorded_queue(&persistence(&shared, channel), channel)
        .unwrap()
        .unwrap();
    assert!(file.versioned);
    assert_eq!(file.items, [75]);
    let [(_, episode, message)] = file.debts.as_slice() else {
        panic!("one debt for the queued original: {:?}", file.debts);
    };
    assert_eq!((episode, *message), (&proof.episode, state.user_msg_id));
    let after = seen(&shared, channel).await;
    assert_eq!(after.queue, [75]);
    assert_eq!(after.record.unwrap().exits, file.debts);
    pool.close().await;
    db.drop().await;
}
