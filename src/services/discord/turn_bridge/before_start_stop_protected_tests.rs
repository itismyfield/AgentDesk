//! A protected channel's policy close: the worker clears the receipt under its step's own scope,
//! a held sidecar refuses at once, and admission and ACK carry the caller's permit to a worker.
use super::*;
use crate::services::discord::input_runtime::fence::{
    self, Failure, Gate, PopulationScope, effect,
};
use crate::services::provider::cancel_token_claude_interrupt::herdr_stop_settlement_available;
use crate::services::turn_orchestrator::policy_close::test_hooks::Judged;

const ALL: [Point; 8] = [
    Point::SubmitAttempt,
    Point::SendAccepted,
    Point::Dequeued,
    Point::MailboxCommitted,
    Point::AckObserved,
    Point::QueueExit,
    Point::FinalizeEntered,
    Point::FinalizeReturned,
];

fn protect(close: &Close) -> (Arc<Gate>, fence::test_health::Clear) {
    let gate = Gate::protect(close.shared.provider.clone(), close.row.channel_id).unwrap();
    let health = fence::test_health::Clear::new(&gate);
    (gate, health)
}

fn sidecar(close: &Close) -> std::path::PathBuf {
    close.path().with_extension("json.lock")
}

/// Whether a second handle's non-blocking lock on `lock` would block.
fn held_elsewhere(lock: &std::path::Path) -> bool {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock)
        .unwrap();
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// Records, at each stage, whether a second handle on the row's sidecar would block.
fn sidecar_held_at_each_stage(
    close: &Close,
    probe: &Probe,
    panic_at: Option<&'static str>,
) -> Arc<Mutex<Vec<(&'static str, bool)>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (slot, lock) = (seen.clone(), sidecar(close));
    probe.on_stage(Box::new(move |stage| {
        if stage == "before_finalize" {
            return;
        }
        let held = held_elsewhere(&lock);
        slot.lock().unwrap().push((stage, held));
        if Some(stage) == panic_at {
            panic!("injected policy close panic at {stage}");
        }
    }));
    seen
}

/// An open protected channel: the worker clears the exact receipt and commits once, keeping the
/// other turn and the queue; every point reads once, a re-ask is a duplicate, the next turn saves.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_s2_protected_open_commit() {
    let close = Close::open(5_341_101, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let (_gate, _health) = protect(&close);
    let intent = close.intent(acked.clone());
    let probe = intent.request.trace.probe();
    assert_eq!(
        (probe.channel, probe.episode.as_str()),
        (close.row.channel_id, close.proof.turn_nonce.as_str())
    );
    let outcome = close.submit(intent).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::Settled),
        "{outcome:?}"
    );
    let here = std::thread::current().id();
    assert!(probe.stages().contains(&"after_commit"));
    let threads = step_threads(&probe);
    assert!(!threads.is_empty() && threads.iter().all(|thread| *thread != here));
    assert_released(&close, &queued, "protected").await;
    assert_eq!(
        probe.snapshot().judge(probe.request, &ALL),
        Ok(vec![1; ALL.len()])
    );
    // Gate open and the input fence passed: the witness answers the re-ask.
    let again = close.submit(close.intent(acked)).await;
    assert!(
        matches!(again, PolicyCloseOutcome::DuplicateConfirmed),
        "{again:?}"
    );
    assert_released(&close, &queued, "duplicate").await;
    let next = Arc::new(CancelToken::new());
    let mut row = close.row.clone();
    row.turn_nonce = next.turn_nonce().map(str::to_owned);
    row.user_msg_id += 10;
    assert!(
        discord::mailbox_try_start_turn_kinded(
            &close.shared,
            close.channel(),
            next,
            UserId::new(2),
            MessageId::new(row.user_msg_id),
            crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
        )
        .await
    );
    let saved = row.clone();
    effect::io(move || discord::inflight::save_inflight_state(&saved))
        .await
        .expect("the next turn saves its own row");
    assert_eq!(close.durable().unwrap().turn_nonce, row.turn_nonce);
}

/// A sidecar another handle holds refuses the close as input-fence `Busy` at once; a snapshot asked
/// next answers while it is still held, nothing changed, and the same close settles after release.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_s2_sidecar_contention_refuses_without_waiting() {
    let close = Close::open(5_341_102, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let before = close.bytes();
    let (_gate, _health) = protect(&close);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let lock = sidecar(&close);
    let holder = std::thread::spawn(move || {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock)
            .unwrap();
        file.lock().unwrap();
        held_tx.send(()).unwrap();
        // A watchdog well past the writer's own wait deadline only bounds a hung run.
        let _ = release_rx.recv_timeout(std::time::Duration::from_secs(12));
        drop(file);
    });
    held_rx.recv().unwrap();
    let intent = close.intent(acked.clone());
    let probe = intent.request.trace.probe();
    let outcome = close.submit(intent).await;
    let snapshot = close.slot().await;
    let still_held = held_elsewhere(&sidecar(&close));
    let _ = release_tx.send(());
    holder.join().unwrap();
    assert!(
        matches!(
            outcome,
            PolicyCloseOutcome::Retained(PolicyRetain::InputFence(Failure::Busy))
        ),
        "{outcome:?}"
    );
    assert!(snapshot.is_some_and(|slot| Arc::ptr_eq(&slot, &close.actor)));
    assert!(still_held, "both answers came while the sidecar was held");
    assert!(probe.stages().is_empty(), "no clear was tried");
    let read = probe
        .snapshot()
        .judge(probe.request, &[Point::Dequeued, Point::AckObserved]);
    assert_eq!(read, Ok(vec![1, 1]));
    assert_held(&close, &before, &queued, "contended").await;
    let outcome = close.submit(close.intent(acked)).await;
    assert!(
        matches!(outcome, PolicyCloseOutcome::Settled),
        "{outcome:?}"
    );
    assert_released(&close, &queued, "after release").await;
}

/// The whole bridge on an open protected channel: receipt admission, notice ACK, the finalizer and
/// the worker's clear, with no receipt planted beforehand.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_s2_protected_bridge_end_to_end() {
    let close = Close::open(5_341_103, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let (_gate, _health) = protect(&close);
    assert!(herdr_stop_settlement_available());
    let seen = armed();
    assert!(close.consume(shown()).await, "the close settles");
    assert!(seen.lock().unwrap().contains(&"after_commit"));
    assert_released(&close, &queued, "bridge").await;
}

/// In `Closing`, only an effect holding a pre-acquired permit may write: the bridge's admission and
/// its ACK each carry the caller's permit to their worker, and without it admission is refused.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_s2_admission_and_ack_carry_the_permit_into_closing() {
    let close = Close::open(5_341_104, |_| {}).await;
    let (gate, _health) = protect(&close);
    let permit = gate.admit().unwrap();
    let _closing = gate.close().unwrap();
    let (nonce, basis) = (close.proof.turn_nonce.clone(), close.proof.basis);
    let generation = close.shared.restart.current_generation;
    let refused = close
        .row
        .admit_before_start_receipt(&nonce, basis, generation)
        .await;
    assert!(refused.is_none(), "no permit, no admission while closing");
    assert_eq!(close.receipt_pending(), None);
    let admit = close
        .row
        .admit_before_start_receipt(&nonce, basis, generation);
    let admitted = effect::scope(Some(permit.clone()), admit).await;
    let admitted = admitted.expect("the permit admits");
    assert_eq!(close.receipt_pending(), Some(true));
    let sent = shown().await;
    let ack = admitted.ack_before_start_notice(&sent);
    assert!(
        effect::scope(Some(permit), ack).await.is_some(),
        "the permit acks"
    );
    assert_eq!(close.receipt_pending(), Some(false));
}

/// With one blocking worker, the settlement switch a request carries to it is the worker's only
/// for that request: after the admission and after the mailbox step it reads its own value again.
#[test]
fn p10_fin_s2_worker_settlement_switch_restored() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .thread_keep_alive(std::time::Duration::from_secs(600))
        .build()
        .unwrap();
    runtime.block_on(async {
        let worker = || async {
            tokio::task::spawn_blocking(|| {
                (
                    std::thread::current().id(),
                    herdr_stop_settlement_available(),
                )
            })
            .await
            .unwrap()
        };
        let close = Close::open(5_341_105, |_| {}).await;
        let queued = close.other_turn_and_queue().await;
        let acked = close.acked().await;
        let before = close.bytes();
        let (_gate, _health) = protect(&close);
        let (thread, on) = worker().await;
        assert!(on, "the worker's own switch");
        let off = step::Settlement::set(false);
        let (nonce, basis) = (close.proof.turn_nonce.clone(), close.proof.basis);
        let generation = close.shared.restart.current_generation;
        let admitted = close
            .row
            .admit_before_start_receipt(&nonce, basis, generation)
            .await;
        assert!(admitted.is_none(), "the worker ran with the caller's off");
        assert_eq!(worker().await, (thread, true), "admission restored it");
        let intent = close.intent(acked.clone());
        drop(off);
        let outcome = close.submit(intent).await;
        assert!(
            matches!(
                outcome,
                PolicyCloseOutcome::Retained(PolicyRetain::Clear(PolicyClear::Mismatch))
            ),
            "the step ran with the request's off: {outcome:?}"
        );
        assert_eq!(worker().await, (thread, true), "the step restored it");
        assert_held(&close, &before, &queued, "off").await;
    });
}

/// The lent authority names one row: another channel, provider or root refuses untouched; nested
/// and stale writers refuse; a legacy step on a protected row is refused by type.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_s2_authority_names_one_row() {
    let close = Close::open(5_341_106, |_| {}).await;
    let acked = close.acked().await;
    let bytes = close.bytes();
    let generation = close.shared.restart.current_generation;
    let (provider, channel) = (close.shared.provider.clone(), close.row.channel_id);
    let root = fence::population_root().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let (gate, _health) = protect(&close);
    let permit = gate.admit().unwrap();
    let neighbour = Gate::protect(provider.clone(), channel + 800).unwrap();
    let codex = Gate::protect(ProviderKind::Codex, channel).unwrap();
    let foreign = [
        (
            "channel",
            root.clone(),
            provider.clone(),
            channel + 800,
            &neighbour,
        ),
        (
            "provider",
            root.clone(),
            ProviderKind::Codex,
            channel,
            &codex,
        ),
        (
            "root",
            elsewhere.path().to_owned(),
            provider.clone(),
            channel,
            &gate,
        ),
    ];
    for (case, root, provider, channel, gate) in foreign {
        let permit = gate.admit().unwrap();
        let scope = PopulationScope::try_writer(&root, &provider, channel, &permit).unwrap();
        let receipt = acked.clone().into_policy_receipt(generation);
        assert_eq!(
            receipt.try_clear(&step::authority(&scope)),
            PolicyClear::InputFence(Failure::StalePermit),
            "{case}"
        );
        assert_eq!(close.bytes(), bytes, "{case}: the row stays");
    }
    let scope = PopulationScope::try_writer(&root, &provider, channel, &permit).unwrap();
    let nested = PopulationScope::try_writer(&root, &provider, channel, &permit);
    assert_eq!(nested.err(), Some(Failure::Busy), "same-thread nesting");
    let receipt = acked.clone().into_policy_receipt(generation);
    assert_eq!(
        receipt.try_clear(&step::legacy()),
        PolicyClear::InputFence(Failure::Busy)
    );
    assert_eq!(close.bytes(), bytes, "legacy on a protected row");
    drop(scope);
    let stale = neighbour.admit().unwrap();
    let refused = PopulationScope::try_writer(&root, &provider, channel, &stale);
    assert_eq!(refused.err(), Some(Failure::StalePermit));
    let scope = PopulationScope::try_writer(&root, &provider, channel, &permit).unwrap();
    assert_eq!(
        receipt.try_clear(&step::authority(&scope)),
        PolicyClear::Cleared
    );
    assert!(close.bytes().is_none(), "the exact scope clears it");
}

/// A legacy step whose channel is protected between its fence and its clear is refused by type at
/// the clear: never an IO failure and never a wait in the protected writer.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_s2_late_protection_is_typed_busy() {
    let close = Close::open(5_341_107, |_| {}).await;
    let queued = close.other_turn_and_queue().await;
    let acked = close.acked().await;
    let before = close.bytes();
    let intent = close.intent(acked);
    let probe = intent.request.trace.probe();
    let (provider, channel) = (close.shared.provider.clone(), close.row.channel_id);
    let protected = Arc::new(Mutex::new(None));
    let slot = protected.clone();
    probe.on_stage(Box::new(move |stage| {
        if stage == "before_clear" {
            *slot.lock().unwrap() = Gate::protect(provider.clone(), channel).ok();
        }
    }));
    let outcome = close.submit(intent).await;
    let gate = protected
        .lock()
        .unwrap()
        .take()
        .expect("protected mid-step");
    let _health = fence::test_health::Clear::new(&gate);
    assert!(
        matches!(
            outcome,
            PolicyCloseOutcome::Retained(PolicyRetain::InputFence(Failure::Busy))
        ),
        "{outcome:?}"
    );
    assert_eq!(
        probe.stages(),
        ["before_claim", "before_clear", "clear_refused"]
    );
    assert_held(&close, &before, &queued, "late protection").await;
}

/// The step's scope holds the sidecar from the clear through the witness, and through an unwind
/// after the clear too.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_s2_authority_spans_the_commit() {
    for (n, panic_at) in [None, Some("after_detach")].into_iter().enumerate() {
        let close = Close::open(5_341_108 + n as u64, |_| {}).await;
        let queued = close.other_turn_and_queue().await;
        let acked = close.acked().await;
        let (_gate, _health) = protect(&close);
        let intent = close.intent(acked);
        let probe = intent.request.trace.probe();
        let seen = sidecar_held_at_each_stage(&close, &probe, panic_at);
        let outcome = close.submit(intent).await;
        assert!(
            matches!(outcome, PolicyCloseOutcome::Settled),
            "{outcome:?}"
        );
        let seen = seen.lock().unwrap().clone();
        let expected: &[&str] = match panic_at {
            None => &[
                "before_claim",
                "before_clear",
                "after_clear",
                "after_detach",
                "after_debit",
                "after_fence",
                "after_commit",
            ],
            Some(_) => &[
                "before_claim",
                "before_clear",
                "after_clear",
                "after_detach",
                "unwound",
            ],
        };
        let stages: Vec<_> = seen.iter().map(|(stage, _)| *stage).collect();
        assert_eq!(stages, expected, "{panic_at:?}");
        assert!(seen.iter().all(|(_, held)| *held), "{panic_at:?}: {seen:?}");
        assert_released(&close, &queued, "spans").await;
    }
}

/// Two channels closing at once each see only their own request; a commit lost between its queue
/// exit and its finalize reads as such, and a re-ask is no finalize or queue exit.
#[tokio::test(flavor = "current_thread")]
async fn p10_fin_s2_probes_and_counts_per_request() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let first = Close::open_in(17, 5_341_110, |_| {}).await;
    let second = Close::open_in(17, 5_341_111, |_| {}).await;
    let (queued_first, queued_second) = (
        first.other_turn_and_queue().await,
        second.other_turn_and_queue().await,
    );
    let (acked_first, acked_second) = (first.acked().await, second.acked().await);
    let (one, two) = (
        first.intent(acked_first.clone()),
        second.intent(acked_second),
    );
    let (probe_one, probe_two) = (one.request.trace.probe(), two.request.trace.probe());
    let (a, b) = tokio::join!(first.submit(one), second.submit(two));
    assert!(matches!(a, PolicyCloseOutcome::Settled), "{a:?}");
    assert!(matches!(b, PolicyCloseOutcome::Settled), "{b:?}");
    for (probe, close) in [(&probe_one, &first), (&probe_two, &second)] {
        assert_eq!(probe.channel, close.row.channel_id);
        assert_eq!(
            probe
                .stages()
                .iter()
                .filter(|s| **s == "after_commit")
                .count(),
            1
        );
        let read = probe
            .snapshot()
            .judge(probe.request, &[Point::Dequeued, Point::MailboxCommitted]);
        assert_eq!(read, Ok(vec![1, 1]));
    }
    assert_ne!(probe_one.request, probe_two.request);
    let foreign = probe_one
        .snapshot()
        .judge(probe_two.request, &[Point::Dequeued]);
    assert_eq!(foreign, Err(Judged::Foreign));
    let missing = probe_one.snapshot().unregistered(Point::QueueExit);
    let missing = missing.judge(probe_one.request, &[Point::QueueExit]);
    assert_eq!(missing, Err(Judged::Missing(Point::QueueExit)));
    let totals = step::totals();
    assert_eq!(
        totals
            .clone()
            .with_run(totals.run + 1)
            .judge(totals.run, &[]),
        Err(Judged::Foreign)
    );
    assert_released(&first, &queued_first, "first").await;
    assert_released(&second, &queued_second, "second").await;
    // A commit whose finalize never started: committed and exited the queue, never finalized.
    let third = Close::open_in(17, 5_341_112, |_| {}).await;
    let queued = third.other_turn_and_queue().await;
    let acked = third.acked().await;
    let intent = third.intent(acked.clone());
    let gap = intent.request.trace.probe();
    panic_once_at(&gap, "before_finalize");
    let outcome = third.submit(intent).await;
    assert!(
        matches!(
            outcome,
            PolicyCloseOutcome::Unconfirmed(PolicyStage::FinalizerPanic)
        ),
        "{outcome:?}"
    );
    assert_released(&third, &queued, "gap").await;
    let points = [
        Point::MailboxCommitted,
        Point::QueueExit,
        Point::FinalizeEntered,
    ];
    assert_eq!(
        gap.snapshot().judge(gap.request, &points),
        Ok(vec![1, 1, 0])
    );
    let intent = third.intent(acked);
    let asked = intent.request.trace.probe();
    let again = third.submit(intent).await;
    assert!(
        matches!(again, PolicyCloseOutcome::DuplicateConfirmed),
        "{again:?}"
    );
    assert_eq!(
        asked.snapshot().judge(asked.request, &points),
        Ok(vec![0, 0, 0])
    );
}
