//! A real Gate lifecycle: a restore retires every handback port, and a reinstall
//! inherits only a settled entry of the same Gate and producer handle, refusing before release.
use super::super::{Gate, test_health};
use super::*;
use crate::services::discord::input_runtime::supervisor::ordering::g1b_tests::entry_state;
use crate::services::discord::input_runtime::supervisor::ordering::{
    AdmissionOrder, HandbackExecution,
};
use std::cell::Cell;

const CLAUDE: ProviderKind = ProviderKind::Claude;

// Close, then the freeze's mode effect (the actor's queue freeze is outside this unit), then Handback.
fn handback(gate: &Arc<Gate>) -> Closing {
    let closing = gate.close().unwrap();
    gate.state.lock().unwrap().mode = Mode::Frozen;
    closing.begin_handback().unwrap();
    closing
}

fn protected(gate: &Gate) -> (Mode, u64, bool) {
    let state = gate.state.lock().unwrap();
    (
        state.mode,
        state.epoch,
        gate.protected.load(Ordering::Acquire),
    )
}

fn producer(channel: u64, pending: &[u64]) -> AdmissionOrder {
    let mut order = AdmissionOrder::new(CLAUDE, channel, 7, 0);
    order.pending(pending);
    order
}

fn scan(channel: u64, sources: Vec<u64>, horizon: u64) -> ordering::ScanCapability {
    let ticket = ordering::handback_fetch_ticket(&CLAUDE, channel).unwrap();
    ordering::handback_begin_scan(ticket, sources, horizon, true).unwrap()
}

fn settle_all(channel: u64, horizon: u64) {
    let proof = ordering::handback_complete(scan(channel, vec![], horizon)).unwrap();
    assert_eq!(settle_order_barrier(proof), Ok(true));
}

fn commit_all(channel: u64, sources: &[u64]) {
    let horizon = *sources.last().unwrap();
    let cap = scan(channel, sources.to_vec(), horizon);
    let executed: HandbackExecution<()> =
        ordering::handback_commit_from_scan(cap, sources, || Ok(())).unwrap();
    assert!(executed.next.is_some());
}

#[test]
fn g1b_restore_invalidates_every_handback_port() {
    let ports = [
        "pending", "ticket", "begin", "permits", "commit", "settle", "defer", "complete", "final",
    ];
    for (index, port) in ports.into_iter().enumerate() {
        let channel = 6_326_150 + index as u64;
        let gate = Gate::protect(CLAUDE, channel).unwrap();
        let _health = test_health::Clear::new(&gate);
        let closing = handback(&gate);
        let pending: &[u64] = if index < 2 { &[] } else { &[11] };
        let snapshot = producer(channel, pending).pending_snapshot();
        closing
            .release_after_handback_with_pending(&snapshot)
            .unwrap();
        if index < 2 {
            settle_all(channel, 0);
        }
        if index >= 7 {
            commit_all(channel, &[11]);
        }
        let ticket = (index == 2).then(|| ordering::handback_fetch_ticket(&CLAUDE, channel));
        let ticket = ticket.map(Result::unwrap);
        let cap = (3..7).contains(&index).then(|| scan(channel, vec![11], 11));
        let ready = (index == 7).then(|| scan(channel, vec![], 11));
        let proof = (index == 8).then(|| ordering::handback_complete(scan(channel, vec![], 11)));
        let proof = proof.map(Result::unwrap);
        // A real restore and a new close move this Gate past the release.
        gate.restore_protection().unwrap();
        let _next = gate.close().unwrap();
        let before = entry_state(channel);
        let calls = Cell::new(0);
        let refused = match port {
            "pending" => ordering::handback_pending(&CLAUDE, channel, &[12]).err(),
            "ticket" => ordering::handback_fetch_ticket(&CLAUDE, channel).err(),
            "begin" => ordering::handback_begin_scan(ticket.unwrap(), vec![11], 11, true).err(),
            "permits" => ordering::handback_permits_sources(&cap.unwrap(), &[11]).err(),
            "commit" => ordering::handback_commit_from_scan(cap.unwrap(), &[11], || {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .err(),
            "settle" => ordering::handback_settle_sources(cap.unwrap(), &[11]).err(),
            "defer" => ordering::handback_defer(&cap.unwrap(), 11).err(),
            "complete" => ordering::handback_complete(ready.unwrap()).err(),
            _ => settle_order_barrier(proof.unwrap()).err(),
        };
        assert_eq!(refused, Some(Failure::StalePermit), "{port}");
        assert_eq!(calls.get(), 0, "{port}");
        assert_eq!(entry_state(channel), before, "{port} changed the entry");
        assert!(order_barrier(&CLAUDE, channel), "{port}");
    }
}

#[test]
fn g1b_unsettled_reinstall_refused_before_release() {
    let channel = 6_326_160;
    let gate = Gate::protect(CLAUDE, channel).unwrap();
    let _health = test_health::Clear::new(&gate);
    let original = producer(channel, &[10]);
    handback(&gate)
        .release_after_handback_with_pending(&original.pending_snapshot())
        .unwrap();
    gate.restore_protection().unwrap();
    let second = handback(&gate);
    let held = protected(&gate);
    let before = entry_state(channel);
    // The new snapshot is empty and shares the handle, yet H1 still owes 10.
    let mut empty = original.pending_snapshot();
    empty.sources.clear();
    assert_eq!(
        second.release_after_handback_with_pending(&empty),
        Err(Failure::StalePermit)
    );
    assert_eq!(protected(&gate), held, "protection, mode and epoch stay");
    assert_eq!(held, (Mode::Handback, held.1, true));
    assert_eq!(entry_state(channel), before);
    assert_eq!(before.0, [10]);
}

#[test]
fn g1b_settled_reinstall_requires_same_overflow() {
    let channel = 6_326_161;
    let gate = Gate::protect(CLAUDE, channel).unwrap();
    let _health = test_health::Clear::new(&gate);
    let mut original = producer(channel, &[]);
    let overflow = original.pending_overflow();
    handback(&gate)
        .release_after_handback_with_pending(&original.pending_snapshot())
        .unwrap();
    ordering::handback_pending(&CLAUDE, channel, &[11]).unwrap();
    commit_all(channel, &[11]);
    settle_all(channel, 11);
    gate.restore_protection().unwrap();
    let second = handback(&gate);
    let held = protected(&gate);
    let regressed = original.pending_snapshot();
    // Same dirty generation as the entry, so only the foreign handle can refuse it.
    let other = producer(channel, &[12]).pending_snapshot();
    let foreign = Closing::frozen_for_test(CLAUDE, channel);
    foreign.begin_handback().unwrap();
    let advanced = original.pending(&[12]);
    for (closing, snapshot) in [
        (&second, &other),
        (&second, &regressed),
        (&*foreign, &advanced),
    ] {
        assert_eq!(
            closing.release_after_handback_with_pending(snapshot),
            Err(Failure::StalePermit)
        );
        assert_eq!(protected(&gate), held, "refused before the release");
    }
    second
        .release_after_handback_with_pending(&advanced)
        .unwrap();
    assert!(!protected(&gate).2);
    commit_all(channel, &[12]);
    settle_all(channel, 12);
    assert!(!order_barrier(&CLAUDE, channel));
    // The first producer's handle still reaches the reinstalled entry.
    overflow.mark_dirty();
    assert!(order_barrier(&CLAUDE, channel));
}

#[test]
fn g1b_restore_serializes_with_durable_callback() {
    use crate::services::discord::input_runtime::supervisor::ordering::CONTENDED;
    use std::sync::mpsc;
    let channel = 6_326_162;
    let gate = Gate::protect(CLAUDE, channel).unwrap();
    let _health = test_health::Clear::new(&gate);
    handback(&gate)
        .release_after_handback_with_pending(&producer(channel, &[]).pending_snapshot())
        .unwrap();
    let cap = scan(channel, vec![11], 11);
    let (events, observed) = mpsc::channel();
    let mut restorer = None;
    let executed = ordering::handback_commit_from_scan(cap, &[11], || {
        // The restore starts while the callback runs; it may only wait, never land, until it ends.
        let (restoring, events) = (gate.clone(), events.clone());
        restorer = Some(std::thread::spawn(move || {
            let waiting = events.clone();
            CONTENDED.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    waiting.send("restore waits").unwrap();
                }))
            });
            let restored = restoring.restore_protection();
            events.send("restore landed").unwrap();
            restored
        }));
        assert_eq!(observed.recv().unwrap(), "restore waits");
        assert!(
            !protected(&gate).2,
            "the release still stands inside the callback"
        );
        Ok("durable")
    })
    .unwrap();
    assert_eq!(
        (executed.outcome, executed.settlement),
        (Ok("durable"), Ok(()))
    );
    assert_eq!(restorer.unwrap().join().unwrap(), Ok(()));
    assert_eq!(observed.recv().unwrap(), "restore landed");
    assert!(protected(&gate).2);
    assert!(ordering::handback_fetch_ticket(&CLAUDE, channel).is_err());
}

#[test]
fn g1b_generation_exhaustion_matrix_gate() {
    // A release one step below the limit issues MAX-1, and the restore after it is refused.
    let channel = 6_326_163;
    let gate = Gate::protect(CLAUDE, channel).unwrap();
    let _health = test_health::Clear::new(&gate);
    gate.state.lock().unwrap().epoch = u64::MAX - 2;
    let snapshot = producer(channel, &[]).pending_snapshot();
    handback(&gate)
        .release_after_handback_with_pending(&snapshot)
        .unwrap();
    assert_eq!(protected(&gate), (Mode::LegacyOpen, u64::MAX - 1, false));
    for _ in 0..2 {
        assert_eq!(gate.restore_protection(), Err(Failure::GenerationExhausted));
        assert_eq!(protected(&gate), (Mode::LegacyOpen, u64::MAX - 1, false));
    }
    // A release at the limit is refused before protection, mode or the entry move.
    let channel = 6_326_164;
    let gate = Gate::protect(CLAUDE, channel).unwrap();
    let _health = test_health::Clear::new(&gate);
    gate.state.lock().unwrap().epoch = u64::MAX - 1;
    let closing = handback(&gate);
    let snapshot = producer(channel, &[]).pending_snapshot();
    for _ in 0..2 {
        assert_eq!(
            closing.release_after_handback_with_pending(&snapshot),
            Err(Failure::GenerationExhausted)
        );
        assert_eq!(protected(&gate), (Mode::Handback, u64::MAX - 1, true));
        assert!(!order_barrier(&CLAUDE, channel), "no entry was installed");
    }
}

#[test]
fn g1b_generation_exhaustion_matrix_gate_freeze() {
    struct Env(Option<std::ffi::OsString>);
    impl Drop for Env {
        fn drop(&mut self) {
            unsafe {
                match &self.0 {
                    Some(old) => std::env::set_var("AGENTDESK_ROOT_DIR", old),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    std::fs::create_dir_all(&parent).unwrap();
    let root = tempfile::tempdir_in(parent).unwrap();
    let _env = Env(std::env::var_os("AGENTDESK_ROOT_DIR"));
    unsafe {
        std::env::set_var("AGENTDESK_ROOT_DIR", root.path());
    }
    let channel = 6_326_165;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let gate = Gate::protect(CLAUDE, channel).unwrap();
            let _health = test_health::Clear::new(&gate);
            gate.state.lock().unwrap().epoch = u64::MAX - 1;
            let closing = Arc::new(gate.close().unwrap());
            let persistence = crate::services::turn_orchestrator::QueuePersistenceContext::new(
                &CLAUDE, "session", None,
            );
            let frozen = shared
                .freeze_legacy_queue(closing.clone(), persistence)
                .await;
            assert_eq!(frozen.err(), Some(Failure::GenerationExhausted));
            assert_eq!(protected(&gate), (Mode::Closing, u64::MAX - 1, true));
            assert_eq!(closing.begin_handback(), Err(Failure::Mode(Mode::Closing)));
        });
}
