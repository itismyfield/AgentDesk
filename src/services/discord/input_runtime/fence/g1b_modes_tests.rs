//! G1b P2-2/F1 on a real Gate lifecycle: a restore retires every handback port, and a reinstall
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
