//! The real handback ports: the same merged prefix retries, an executed
//! callback keeps its result apart from settlement, and no generation is reused or MAX.
use super::*;
use crate::services::discord::input_runtime::fence::{Closing, Mode, modes};
use std::cell::Cell;

fn producer(channel: u64) -> AdmissionOrder {
    AdmissionOrder::new(ProviderKind::Claude, channel, 7, 0)
}

// Releases a frozen Closing with an empty snapshot that shares `order`'s overflow handle.
fn release(channel: u64, order: &AdmissionOrder) {
    let closing = Closing::frozen_for_test(ProviderKind::Claude, channel);
    closing.begin_handback().unwrap();
    (closing.release_after_handback_with_pending(&order.pending_snapshot())).unwrap();
}

fn scan(channel: u64, sources: Vec<u64>, horizon: u64) -> Result<ScanCapability, Failure> {
    let ticket = handback_fetch_ticket(&ProviderKind::Claude, channel)?;
    handback_begin_scan(ticket, sources, horizon, true)
}

fn commit<T>(
    cap: ScanCapability,
    sources: &[u64],
    calls: &Cell<u32>,
    result: impl FnOnce() -> Result<T, Failure>,
) -> Result<HandbackExecution<T>, Failure> {
    handback_commit_from_scan(cap, sources, || {
        calls.set(calls.get() + 1);
        result()
    })
}

fn settle_all(channel: u64, horizon: u64) {
    let proof = handback_complete(scan(channel, vec![], horizon).unwrap()).unwrap();
    assert_eq!(modes::settle_order_barrier(proof), Ok(true));
}

fn with_entry<T>(channel: u64, action: impl FnOnce(&mut HandbackEntry) -> T) -> T {
    let mut entries = BARRIERS.lock().unwrap();
    action(entries.get_mut(&("claude".to_owned(), channel)).unwrap())
}

pub(crate) type EntryState = (Vec<u64>, Option<u64>, u64, u64, u64, Option<u64>);

/// A handback entry's retry state and settled marker, compared around a refused port.
pub(crate) fn entry_state(channel: u64) -> EntryState {
    with_entry(channel, |entry| {
        let order = &entry.order;
        let pending = order.pending.iter().copied().collect();
        let stamp = &order.stamp;
        (
            pending,
            order.barrier,
            stamp.frontier,
            stamp.scan,
            stamp.dirty,
            entry.settled_overflow,
        )
    })
}

/// Seeds an order one step below dirty exhaustion.
pub(crate) fn exhaust_dirty(order: &mut AdmissionOrder) {
    order.stamp.dirty = u64::MAX - 1;
}

#[test]
fn g1b_handback_retries_same_merged_prefix() {
    let channel = 6_326_130;
    release(channel, &producer(channel));
    let calls = Cell::new(0);
    let cap = scan(channel, vec![10, 11, 12], 12).unwrap();
    let failed = commit(cap, &[10, 11], &calls, || {
        Err::<(), _>(Failure::Persistence)
    })
    .unwrap();
    assert_eq!(failed.outcome, Err(Failure::Persistence));
    assert_eq!(
        failed.settlement,
        Ok(()),
        "the first deferral itself succeeds"
    );
    assert!(failed.next.is_none());
    let cap = scan(channel, vec![10, 11, 12], 12).unwrap();
    let retried = commit(cap, &[10, 11], &calls, || Ok("merged")).unwrap();
    assert_eq!(
        (retried.outcome, retried.settlement),
        (Ok("merged"), Ok(()))
    );
    let after = commit(retried.next.unwrap(), &[12], &calls, || Ok("next")).unwrap();
    assert_eq!(after.outcome, Ok("next"));
    assert_eq!(
        calls.get(),
        3,
        "the merged retry ran once more, then 12 followed"
    );
    settle_all(channel, 12);
}

#[test]
fn g1b_prefix_barrier_negative_and_older_source() {
    let channel = 6_326_131;
    release(channel, &producer(channel));
    let calls = Cell::new(0);
    let cap = scan(channel, vec![10, 11], 11).unwrap();
    let failed = commit(cap, &[10, 11], &calls, || {
        Err::<(), _>(Failure::Persistence)
    })
    .unwrap();
    assert_eq!(failed.settlement, Ok(()));
    // Skipping the deferred head, omitting a capability source or crossing the horizon runs nothing.
    assert!(scan(channel, vec![11], 11).is_err());
    for sources in [vec![11], vec![10, 12], vec![10, 11, 12]] {
        let cap = scan(channel, vec![10, 11], 11).unwrap();
        assert!(
            commit(cap, &sources, &calls, || Ok(())).is_err(),
            "{sources:?}"
        );
    }
    assert_eq!(calls.get(), 1);
    // An older source found by a later scan settles first without naming the barrier.
    let cap = scan(channel, vec![5, 10, 11], 11).unwrap();
    let older = commit(cap, &[5], &calls, || Ok("older")).unwrap();
    assert_eq!((older.outcome, older.settlement), (Ok("older"), Ok(())));
    let cap = older.next.unwrap();
    assert!(
        handback_permits_sources(&cap, &[5, 10]).is_err(),
        "5 is settled"
    );
    let retried = commit(cap, &[10, 11], &calls, || Ok("merged")).unwrap();
    assert!(retried.next.is_some());
    assert_eq!(calls.get(), 3);
    settle_all(channel, 11);
}

#[test]
fn g1b_handback_returns_durable_token_on_settlement_failure() {
    let channel = 6_326_132;
    let original = producer(channel);
    let overflow = original.pending_overflow();
    release(channel, &original);
    let calls = Cell::new(0);
    let cap = scan(channel, vec![10, 11], 11).unwrap();
    let executed = commit(cap, &[10, 11], &calls, || {
        overflow.mark_dirty();
        Ok("durable token")
    })
    .unwrap();
    assert_eq!(executed.outcome, Ok("durable token"));
    assert_eq!(executed.settlement, Err(Failure::StalePermit));
    assert!(executed.next.is_none());
    // The pending set was empty, so only the execution path can still hold 10 and 11.
    assert!(modes::order_barrier(&ProviderKind::Claude, channel));
    for (sources, horizon) in [(vec![], 11), (vec![10], 11), (vec![11], 11)] {
        assert!(scan(channel, sources, horizon).is_err());
    }
    let cap = scan(channel, vec![10, 11], 11).unwrap();
    assert!(
        commit(cap, &[10, 11], &calls, || Ok(()))
            .unwrap()
            .next
            .is_some()
    );
    settle_all(channel, 11);
}

#[test]
fn g1b_handback_preserves_execution_and_persistence_cause() {
    let channel = 6_326_133;
    let original = producer(channel);
    let overflow = original.pending_overflow();
    release(channel, &original);
    let calls = Cell::new(0);
    // A refusal before the callback is the outer error and runs nothing.
    let stale = scan(channel, vec![10, 11], 11).unwrap();
    handback_pending(&ProviderKind::Claude, channel, &[10]).unwrap();
    assert_eq!(
        commit(stale, &[10, 11], &calls, || Ok(())).err(),
        Some(Failure::StalePermit)
    );
    assert_eq!(calls.get(), 0);
    let cap = scan(channel, vec![10, 11], 11).unwrap();
    let executed = commit(cap, &[10, 11], &calls, || {
        overflow.mark_dirty();
        Err::<(), _>(Failure::Persistence)
    })
    .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(executed.outcome, Err(Failure::Persistence));
    assert_eq!(executed.settlement, Err(Failure::StalePermit));
    assert!(executed.next.is_none());
    assert!(
        scan(channel, vec![10], 11).is_err(),
        "11 stays an obligation"
    );
    let cap = scan(channel, vec![10, 11], 11).unwrap();
    assert!(
        commit(cap, &[10, 11], &calls, || Ok(()))
            .unwrap()
            .next
            .is_some()
    );
    settle_all(channel, 11);
}

#[test]
fn g1b_generation_exhaustion_matrix_scan() {
    let mut order = producer(6_326_134);
    order.stamp.scan = u64::MAX - 2;
    let last = order
        .fetch_ticket(7)
        .expect("MAX-1 is the last scan generation");
    assert_eq!(last.stamp.scan, u64::MAX - 1);
    for _ in 0..2 {
        assert_eq!(order.fetch_ticket(7).err(), Some(EXHAUSTED));
    }
    assert_eq!(order.stamp.scan, u64::MAX - 1, "MAX is never issued");
    assert!(order.begin_scan(last, 7, vec![], 0, true).is_err());
    assert!(!order.offer_ready());
    // invalidate and defer at the limit latch rather than leaving old capabilities current.
    let mut order = producer(6_326_135);
    let ticket = order.fetch_ticket(7).unwrap();
    let mut cap = order.begin_scan(ticket, 7, vec![11], 11, true).unwrap();
    // The order and its capability both sit one step below the limit.
    order.stamp.scan = u64::MAX - 1;
    cap.ticket.stamp.scan = u64::MAX - 1;
    order.defer(&cap, 7, 11).unwrap();
    assert_eq!(
        order.pending_snapshot().sources,
        [11],
        "the deferral keeps its source"
    );
    assert_eq!(order.permits(&cap, 7, 11), Err(Failure::StalePermit));
    assert!(order.fetch_ticket(7).is_err());
    let mut order = producer(6_326_136);
    order.stamp.scan = u64::MAX - 1;
    order.invalidate(8);
    assert!(order.fetch_ticket(8).is_err());
}

#[test]
fn g1b_generation_exhaustion_matrix_dirty() {
    let mut order = producer(6_326_137);
    order.stamp.dirty = u64::MAX - 2;
    assert_eq!(
        order.admit_pending(&[11]).unwrap().dirty_generation,
        u64::MAX - 1
    );
    for _ in 0..2 {
        assert_eq!(order.admit_pending(&[12]).err(), Some(EXHAUSTED));
        let snapshot = order.pending_snapshot();
        assert_eq!(
            (snapshot.sources, snapshot.dirty_generation),
            (vec![11], u64::MAX - 1)
        );
    }
    // The namespace verdict still comes first and stays distinct from exhaustion.
    assert_eq!(
        order.admit_pending(&[11, 0]).err(),
        Some(Failure::StalePermit)
    );
    // A retry obligation the order already checked is kept even when it cannot advance dirty.
    let kept = order.pending(&[12]);
    assert_eq!(kept.sources, [12]);
    assert_eq!(order.pending_snapshot().sources, [11, 12]);
    assert_eq!(order.stamp.dirty, u64::MAX - 1, "MAX is never issued");
    assert!(order.fetch_ticket(7).is_err());
    // The shared overflow saturates on its own and also refuses a fresh fetch.
    let mut shared = producer(6_326_138);
    shared
        .overflow
        .generation
        .store(u64::MAX - 1, Ordering::Release);
    shared.overflow.mark_dirty();
    shared.overflow.mark_dirty();
    assert_eq!(shared.overflow.generation(), u64::MAX);
    assert!(shared.fetch_ticket(7).is_err());
    assert!(!shared.offer_ready());
}

#[test]
fn g1b_generation_exhaustion_matrix_handback() {
    // The allocator is process-global, so the exhaustion runs alone in a child process.
    if std::env::var("ADK_G1B_EXHAUST_CHILD").as_deref() != Ok("1") {
        let name = format!(
            "{}::g1b_generation_exhaustion_matrix_handback",
            module_path!()
        );
        let name = name.split_once("::").unwrap().1;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--nocapture", "--test-threads=1"])
            .env("ADK_G1B_EXHAUST_CHILD", "1")
            .output()
            .unwrap();
        let result = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{result}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(result.contains("1 passed; 0 failed; 0 ignored"), "{result}");
        return;
    }
    let channel = 6_326_139;
    NEXT_HANDBACK.store(u64::MAX - 2, Ordering::Release);
    let original = producer(channel);
    release(channel, &original);
    assert_eq!(
        with_entry(channel, |entry| entry.order.stamp.handback),
        Some(u64::MAX - 2)
    );
    settle_all(channel, 0);
    // The last generation was issued; a later release is refused before protection moves.
    let next = 6_326_140;
    for _ in 0..2 {
        let closing = Closing::frozen_for_test(ProviderKind::Claude, next);
        closing.begin_handback().unwrap();
        let snapshot = producer(next).pending_snapshot();
        assert_eq!(
            closing.release_after_handback_with_pending(&snapshot),
            Err(EXHAUSTED)
        );
        assert!(!modes::order_barrier(&ProviderKind::Claude, next));
        assert_eq!(closing.begin_handback(), Err(Failure::Mode(Mode::Handback)));
    }
    assert_eq!(NEXT_HANDBACK.load(Ordering::Acquire), u64::MAX - 1);
    // A settle at the scan limit latches the entry, so its barrier and ports stay closed.
    with_entry(channel, |entry| entry.order.stamp.scan = u64::MAX - 2);
    let cap = scan(channel, vec![], 0).unwrap();
    let proof = handback_complete(cap).unwrap();
    assert_eq!(modes::settle_order_barrier(proof), Ok(true));
    assert!(modes::order_barrier(&ProviderKind::Claude, channel));
    assert_eq!(scan(channel, vec![], 0).err(), Some(EXHAUSTED));
}
