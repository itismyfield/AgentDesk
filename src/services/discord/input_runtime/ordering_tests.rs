use super::*;
use crate::services::discord::input_runtime::fence::{Closing, modes};

fn order(channel: u64) -> AdmissionOrder {
    AdmissionOrder::new(ProviderKind::Claude, channel, 7, 10)
}

fn begin(
    order: &mut AdmissionOrder,
    epoch: u64,
    sources: Vec<u64>,
    horizon: u64,
    complete: bool,
) -> Result<ScanCapability, Failure> {
    let ticket = order.fetch_ticket(epoch)?;
    order.begin_scan(ticket, epoch, sources, horizon, complete)
}

fn scan(
    order: &mut AdmissionOrder,
    epoch: u64,
    sources: Vec<u64>,
    horizon: u64,
    complete: bool,
) -> ScanCapability {
    begin(order, epoch, sources, horizon, complete).unwrap()
}

fn release(channel: u64, pending: &[u64], dirty: u64) {
    let closing = Closing::frozen_for_test(ProviderKind::Claude, channel);
    closing.begin_handback().unwrap();
    closing.release_after_handback(pending, dirty).unwrap();
}

fn handback_scan(channel: u64, sources: Vec<u64>, horizon: u64, complete: bool) -> ScanCapability {
    let ticket = handback_fetch_ticket(&ProviderKind::Claude, channel).unwrap();
    handback_begin_scan(ticket, sources, horizon, complete).unwrap()
}

#[test]
fn g1a_scan_requires_oldest_first_pending_coverage_and_exact_next_source() {
    let mut order = order(6_325_710);
    let hint = order.pending(&[11, 12]);
    assert_eq!(hint.sources, [11, 12]);
    assert_eq!(hint.dirty_generation, 1);
    for (sources, horizon) in [
        (vec![12, 11], 12),
        (vec![12], 12),
        (vec![10, 11, 12], 12),
        (vec![11, 12], 11),
        (vec![], 9),
    ] {
        assert!(begin(&mut order, 7, sources, horizon, true).is_err());
    }
    let mut cap = scan(&mut order, 7, vec![11, 12], 12, true);
    assert_eq!(order.permits(&cap, 7, 12), Err(Failure::StalePermit));
    order.settle(&mut cap, 7, 11).unwrap();
    assert_eq!(order.stamp.frontier, 11);
    assert_eq!(order.permits(&cap, 7, 12), Ok(()));
    order.settle(&mut cap, 7, 12).unwrap();
    assert!(order.complete(cap, 7).is_ok());
    assert!(order.pending(&[11, 12]).sources.is_empty());
    assert_eq!(order.stamp.dirty, 1);
}

#[test]
fn g1a_deferred_head_cannot_be_overtaken_in_this_or_a_later_scan() {
    let mut order = order(6_325_711);
    let cap = scan(&mut order, 7, vec![11, 12], 12, true);
    order.defer(&cap, 7, 11).unwrap();
    assert_eq!(order.permits(&cap, 7, 12), Err(Failure::StalePermit));
    assert!(begin(&mut order, 7, vec![12], 12, true).is_err());
    let mut retry = scan(&mut order, 7, vec![11, 12], 12, true);
    order.settle(&mut retry, 7, 11).unwrap();
    order.settle(&mut retry, 7, 12).unwrap();
    assert!(order.complete(retry, 7).is_ok());
}

#[test]
fn g1a_an_older_deferred_head_does_not_erase_the_previous_retry_obligation() {
    let mut order = AdmissionOrder::new(ProviderKind::Claude, 6_325_719, 7, 0);
    let cap = scan(&mut order, 7, vec![20, 30], 30, true);
    order.defer(&cap, 7, 20).unwrap();
    let older = scan(&mut order, 7, vec![10, 20], 20, true);
    order.defer(&older, 7, 10).unwrap();
    let mut retry = scan(&mut order, 7, vec![10], 10, true);
    order.settle(&mut retry, 7, 10).unwrap();
    assert!(begin(&mut order, 7, vec![30], 30, true).is_err());
    let mut retry = scan(&mut order, 7, vec![20, 30], 30, true);
    order.settle(&mut retry, 7, 20).unwrap();
    order.settle(&mut retry, 7, 30).unwrap();
    assert!(order.complete(retry, 7).is_ok());
}

#[test]
fn g1a_epoch_dirty_scan_and_scope_changes_refuse_stale_commits() {
    let mut order = order(6_325_712);
    let cap = scan(&mut order, 7, vec![11], 11, true);
    assert_eq!(order.permits(&cap, 8, 11), Err(Failure::StalePermit));
    let mut other = AdmissionOrder::new(ProviderKind::Claude, 6_325_712, 7, 10);
    let _other_cap = scan(&mut other, 7, vec![11], 11, true);
    assert_eq!(other.permits(&cap, 7, 11), Err(Failure::StalePermit));
    order.pending(&[12]);
    assert_eq!(order.permits(&cap, 7, 11), Err(Failure::StalePermit));
    let cap = scan(&mut order, 7, vec![11, 12], 12, true);
    let fresh = scan(&mut order, 7, vec![11, 12], 12, true);
    assert_eq!(order.permits(&cap, 7, 11), Err(Failure::StalePermit));
    order.invalidate(8);
    assert_eq!(order.permits(&fresh, 8, 11), Err(Failure::StalePermit));
    assert!(order.fetch_ticket(7).is_err());
    let cap = scan(&mut order, 8, vec![11, 12], 12, true);
    assert_eq!(order.permits(&cap, 8, 11), Ok(()));
}

#[test]
fn g1a_capability_checks_provider_channel_owner_and_named_epoch() {
    let mut order = order(6_325_717);
    for field in 0..4 {
        let mut cap = scan(&mut order, 7, vec![11], 11, true);
        match field {
            0 => cap.ticket.stamp.provider = ProviderKind::Codex,
            1 => cap.ticket.stamp.channel += 1,
            2 => cap.ticket.stamp.owner += 1,
            _ => cap.ticket.stamp.epoch += 1,
        }
        assert_eq!(order.permits(&cap, 7, 11), Err(Failure::StalePermit));
    }
}

#[test]
fn g1a_fetch_ticket_rejects_clear_binding_dirty_and_new_fetch_boundaries() {
    for boundary in 0..3 {
        let mut order = order(6_325_740 + boundary);
        let ticket = order.fetch_ticket(7).unwrap();
        match boundary {
            0 => order.invalidate(8),
            1 => {
                order.pending(&[11]);
            }
            _ => {
                order.fetch_ticket(7).unwrap();
            }
        }
        let epoch = order.stamp.epoch;
        assert!(order.begin_scan(ticket, epoch, vec![11], 11, true).is_err());
        let mut cap = scan(&mut order, epoch, vec![11], 11, true);
        order.settle(&mut cap, epoch, 11).unwrap();
        assert!(order.complete(cap, epoch).is_ok());
    }
}

#[test]
fn g1a_whole_receipt_sources_must_be_the_exact_unsettled_prefix() {
    let mut order = order(6_325_743);
    order.pending(&[11, 12, 13]);
    let mut cap = scan(&mut order, 7, vec![11, 12, 13], 13, true);
    for sources in [
        vec![11, 13],
        vec![11, 12, 14],
        vec![10, 11],
        vec![12],
        vec![],
    ] {
        assert_eq!(
            order.permits_sources(&cap, 7, &sources),
            Err(Failure::StalePermit)
        );
    }
    assert_eq!(order.permits_sources(&cap, 7, &[11, 12]), Ok(()));
    order.settle_sources(&mut cap, 7, &[11, 12]).unwrap();
    assert_eq!(order.stamp.frontier, 12);
    assert_eq!(order.pending_snapshot().sources, [13]);
    order.settle(&mut cap, 7, 13).unwrap();
    assert!(order.complete(cap, 7).is_ok());
}

#[test]
fn g1a_source_range_cannot_cross_the_verified_horizon() {
    let mut order = order(6_325_744);
    let mut cap = scan(&mut order, 7, vec![11, 12], 12, true);
    cap.horizon = 11;
    assert_eq!(
        order.permits_sources(&cap, 7, &[11, 12]),
        Err(Failure::StalePermit)
    );
}

#[test]
fn g1a_complete_requires_full_horizon_all_pending_and_no_unsettled_page() {
    let mut order = order(6_325_713);
    order.pending(&[12]);
    let mut cap = scan(&mut order, 7, vec![11], 11, true);
    order.settle(&mut cap, 7, 11).unwrap();
    assert!(order.complete(cap, 7).is_err());
    let cap = scan(&mut order, 7, vec![12], 12, true);
    assert!(order.complete(cap, 7).is_err());
    let mut cap = scan(&mut order, 7, vec![12], 12, false);
    order.settle(&mut cap, 7, 12).unwrap();
    assert!(order.complete(cap, 7).is_err());
    let cap = scan(&mut order, 7, vec![], 12, true);
    order.pending(&[13]);
    assert!(order.complete(cap, 7).is_err());
    let mut cap = scan(&mut order, 7, vec![13], 13, true);
    order.settle(&mut cap, 7, 13).unwrap();
    assert!(order.complete(cap, 7).is_ok());
}

#[test]
fn g1a_handback_transfers_pending_and_dirty_into_post_release_authority() {
    let channel = 6_325_745;
    let mut supervisor = order(channel);
    supervisor.pending(&[11, 12]);
    let snapshot = supervisor.pending_snapshot();
    release(channel, &snapshot.sources, snapshot.dirty_generation);
    drop(supervisor);
    let ticket = handback_fetch_ticket(&ProviderKind::Claude, channel).unwrap();
    assert_eq!(ticket.stamp.dirty, snapshot.dirty_generation);
    assert_eq!(
        ticket.stamp.epoch, 2,
        "release epoch is independent of supervisor admission generation"
    );
    assert!(handback_begin_scan(ticket, vec![12], 12, true).is_err());
    let cap = handback_scan(channel, vec![11, 12], 12, true);
    assert_eq!(
        handback_permits_sources(&cap, &[12]),
        Err(Failure::StalePermit)
    );
    let cap = handback_settle_sources(cap, &[11, 12]).unwrap();
    assert!(handback_complete(cap).is_err());
    let cap = handback_scan(channel, vec![], 12, true);
    let proof = handback_complete(cap).unwrap();
    assert_eq!(modes::settle_order_barrier(proof), Ok(true));
}

#[test]
fn g1a_handback_requires_complete_fetch_after_known_pending_settles() {
    let channel = 6_325_718;
    release(channel, &[11], 5);
    let early = handback_fetch_ticket(&ProviderKind::Claude, channel).unwrap();
    let cap = handback_scan(channel, vec![11], 11, true);
    let cap = handback_settle_sources(cap, &[11]).unwrap();
    assert!(handback_complete(cap).is_err());
    assert!(handback_begin_scan(early, vec![], 11, true).is_err());
    assert!(modes::order_barrier(&ProviderKind::Claude, channel));
    let cap = handback_scan(channel, vec![], 11, true);
    let after = handback_complete(cap).unwrap();
    assert_eq!(modes::settle_order_barrier(after), Ok(true));
}

#[test]
fn g1a_post_release_live_dirty_change_rejects_an_in_flight_fetch() {
    let channel = 6_325_746;
    release(channel, &[], 9);
    let ticket = handback_fetch_ticket(&ProviderKind::Claude, channel).unwrap();
    let pending = handback_pending(&ProviderKind::Claude, channel, &[11]).unwrap();
    assert_eq!(pending.dirty_generation, 10);
    assert!(handback_begin_scan(ticket, vec![11], 11, true).is_err());
    let cap = handback_scan(channel, vec![11], 11, true);
    let cap = handback_settle_sources(cap, &[11]).unwrap();
    assert!(handback_complete(cap).is_err());
    let cap = handback_scan(channel, vec![], 11, true);
    assert_eq!(
        modes::settle_order_barrier(handback_complete(cap).unwrap()),
        Ok(true)
    );
}

#[test]
fn g1a_post_release_pending_outside_horizon_still_prevents_completion() {
    let channel = 6_325_747;
    release(channel, &[12], 7);
    let cap = handback_scan(channel, vec![11], 11, true);
    let cap = handback_settle_sources(cap, &[11]).unwrap();
    assert!(handback_complete(cap).is_err());
    let cap = handback_scan(channel, vec![12], 12, true);
    let cap = handback_settle_sources(cap, &[12]).unwrap();
    assert!(handback_complete(cap).is_err());
    let cap = handback_scan(channel, vec![], 12, true);
    assert_eq!(
        modes::settle_order_barrier(handback_complete(cap).unwrap()),
        Ok(true)
    );
}

#[test]
fn g1a_post_release_ticket_requires_its_exact_release_epoch() {
    let channel = 6_325_748;
    release(channel, &[], 0);
    let mut ticket = handback_fetch_ticket(&ProviderKind::Claude, channel).unwrap();
    ticket.stamp.epoch -= 1;
    assert!(handback_begin_scan(ticket, vec![], 0, true).is_err());
    let cap = handback_scan(channel, vec![], 0, true);
    assert_eq!(
        modes::settle_order_barrier(handback_complete(cap).unwrap()),
        Ok(true)
    );
}

#[test]
fn g1a_deferred_post_release_head_requires_a_subsequent_complete_fetch() {
    let channel = 6_325_749;
    release(channel, &[], 0);
    let cap = handback_scan(channel, vec![11], 11, true);
    handback_defer(&cap, 11).unwrap();
    assert!(handback_permits_sources(&cap, &[11]).is_err());
    let cap = handback_scan(channel, vec![11], 11, true);
    let cap = handback_settle_sources(cap, &[11]).unwrap();
    assert!(handback_complete(cap).is_err());
    let cap = handback_scan(channel, vec![], 11, true);
    assert_eq!(
        modes::settle_order_barrier(handback_complete(cap).unwrap()),
        Ok(true)
    );
}

#[test]
fn g1a_handback_durable_callback_is_atomic_with_range_validation_and_settlement() {
    use std::cell::Cell;
    let channel = 6_325_750;
    release(channel, &[11, 12, 13], 4);
    let calls = Cell::new(0);
    let cap = handback_scan(channel, vec![11, 12, 13], 13, true);
    let refused = handback_commit_from_scan(cap, &[11, 13], || {
        calls.set(calls.get() + 1);
        Ok(())
    });
    assert!(refused.is_err());
    assert_eq!(
        calls.get(),
        0,
        "invalid range must not invoke durable commit"
    );
    let cap = handback_scan(channel, vec![11, 12, 13], 13, true);
    let failed = handback_commit_from_scan(cap, &[11], || {
        calls.set(calls.get() + 1);
        Err::<(), _>(Failure::Persistence)
    });
    assert!(matches!(failed, Err(Failure::Persistence)));
    assert_eq!(calls.get(), 1);
    let cap = handback_scan(channel, vec![11, 12, 13], 13, true);
    assert_eq!(
        cap.ticket.stamp.frontier, 0,
        "failed commit settles no source"
    );
    let (committed, cap) = handback_commit_from_scan(cap, &[11], || {
        calls.set(calls.get() + 1);
        Ok("first durable")
    })
    .unwrap();
    assert_eq!(committed, "first durable");
    assert_eq!(handback_permits_sources(&cap, &[12, 13]), Ok(()));
    let (committed, cap) = handback_commit_from_scan(cap, &[12, 13], || {
        calls.set(calls.get() + 1);
        Ok("rest durable")
    })
    .unwrap();
    assert_eq!(committed, "rest durable");
    assert_eq!(calls.get(), 3);
    assert!(handback_complete(cap).is_err());
    let cap = handback_scan(channel, vec![], 13, true);
    assert_eq!(
        modes::settle_order_barrier(handback_complete(cap).unwrap()),
        Ok(true)
    );
}

#[test]
fn g1a_overflow_generation_rejects_fetch_commit_and_complete_without_other_state_changes() {
    let mut order = order(6_325_763);
    let overflow = order.pending_overflow();
    let ticket = order.fetch_ticket(7).unwrap();
    overflow.mark_dirty();
    assert!(order.begin_scan(ticket, 7, vec![11], 11, true).is_err());
    let cap = scan(&mut order, 7, vec![11], 11, true);
    overflow.mark_dirty();
    assert_eq!(order.permits(&cap, 7, 11), Err(Failure::StalePermit));
    let cap = scan(&mut order, 7, vec![], 10, true);
    overflow.mark_dirty();
    assert!(order.complete(cap, 7).is_err());
    let cap = scan(&mut order, 7, vec![], 10, true);
    assert!(order.complete(cap, 7).is_ok());
}

#[test]
fn g1a_overflow_is_owner_scoped_and_saturation_never_reopens_admission() {
    let mut first = order(6_325_764);
    let mut other = order(6_325_765);
    let first_cap = scan(&mut first, 7, vec![], 10, true);
    let other_cap = scan(&mut other, 7, vec![], 10, true);
    first.overflow.mark_dirty();
    assert!(first.complete(first_cap, 7).is_err());
    assert!(other.complete(other_cap, 7).is_ok());
    first
        .overflow
        .generation
        .store(u64::MAX - 1, Ordering::Release);
    let ticket = first.fetch_ticket(7).unwrap();
    first.overflow.mark_dirty();
    first.overflow.mark_dirty();
    assert_eq!(first.overflow.generation(), u64::MAX);
    assert!(first.begin_scan(ticket, 7, vec![], 10, true).is_err());
    assert!(first.fetch_ticket(7).is_err());
}

#[test]
fn g1a_shared_overflow_invalidates_final_handback_and_rearms_after_settlement() {
    let channel = 6_325_766;
    let original = order(channel);
    let overflow = original.pending_overflow();
    let snapshot = original.pending_snapshot();
    let closing = Closing::frozen_for_test(ProviderKind::Claude, channel);
    closing.begin_handback().unwrap();
    closing
        .release_after_handback_with_pending(&snapshot)
        .unwrap();
    drop(original);
    let proof = handback_complete(handback_scan(channel, vec![], 10, true)).unwrap();
    overflow.mark_dirty();
    assert_eq!(
        modes::settle_order_barrier(proof),
        Err(Failure::StalePermit)
    );
    assert!(modes::order_barrier(&ProviderKind::Claude, channel));
    let proof = handback_complete(handback_scan(channel, vec![], 10, true)).unwrap();
    assert_eq!(modes::settle_order_barrier(proof), Ok(true));
    assert!(!modes::order_barrier(&ProviderKind::Claude, channel));
    overflow.mark_dirty();
    assert!(modes::order_barrier(&ProviderKind::Claude, channel));
    let proof = handback_complete(handback_scan(channel, vec![], 10, true)).unwrap();
    assert_eq!(modes::settle_order_barrier(proof), Ok(true));
}

#[test]
fn g1a_shared_overflow_during_durable_callback_settles_zero_and_keeps_barrier() {
    let channel = 6_325_767;
    let original = order(channel);
    let overflow = original.pending_overflow();
    let snapshot = original.pending_snapshot();
    let closing = Closing::frozen_for_test(ProviderKind::Claude, channel);
    closing.begin_handback().unwrap();
    closing
        .release_after_handback_with_pending(&snapshot)
        .unwrap();
    let cap = handback_scan(channel, vec![11], 11, true);
    let calls = std::cell::Cell::new(0);
    let result = handback_commit_from_scan(cap, &[11], || {
        calls.set(calls.get() + 1);
        overflow.mark_dirty();
        Ok(())
    });
    assert!(matches!(result, Err(Failure::StalePermit)));
    assert_eq!(calls.get(), 1);
    assert!(modes::order_barrier(&ProviderKind::Claude, channel));
    let ticket = handback_fetch_ticket(&ProviderKind::Claude, channel).unwrap();
    assert!(handback_begin_scan(ticket, vec![12], 12, true).is_err());
    let cap = handback_scan(channel, vec![11], 11, true);
    assert_eq!(cap.ticket.stamp.frontier, 0);
    let (_, cap) = handback_commit_from_scan(cap, &[11], || Ok(())).unwrap();
    assert!(handback_complete(cap).is_err());
    let cap = handback_scan(channel, vec![], 11, true);
    assert_eq!(
        modes::settle_order_barrier(handback_complete(cap).unwrap()),
        Ok(true)
    );
}

#[test]
fn g1a_handback_rejects_another_channels_overflow_handle_before_release() {
    let original = order(6_325_768);
    let snapshot = original.pending_snapshot();
    let channel = 6_325_769;
    let closing = Closing::frozen_for_test(ProviderKind::Claude, channel);
    closing.begin_handback().unwrap();
    assert_eq!(
        closing.release_after_handback_with_pending(&snapshot),
        Err(Failure::StalePermit)
    );
    assert!(!modes::order_barrier(&ProviderKind::Claude, channel));
    closing.release_after_handback(&[], 0).unwrap();
    let proof = handback_complete(handback_scan(channel, vec![], 0, true)).unwrap();
    assert_eq!(modes::settle_order_barrier(proof), Ok(true));
}

#[test]
fn g1a_offer_requires_settled_pending_and_verified_overflow_with_initial_zero_compatibility() {
    let mut order = order(6_325_772);
    assert!(order.offer_ready());
    order.pending(&[11]);
    assert!(!order.offer_ready());
    let mut cap = scan(&mut order, 7, vec![11], 11, true);
    order.settle(&mut cap, 7, 11).unwrap();
    assert!(
        order.offer_ready(),
        "initial overflow 0 preserves existing offer behavior"
    );
    order.overflow.mark_dirty();
    assert!(!order.offer_ready());
    let cap = scan(&mut order, 7, vec![], 11, false);
    assert!(order.complete(cap, 7).is_err());
    assert!(!order.offer_ready());
    let cap = scan(&mut order, 7, vec![], 11, true);
    assert!(!order.offer_ready(), "fetch alone is not complete evidence");
    order.complete(cap, 7).unwrap();
    assert!(order.offer_ready());
    order.overflow.mark_dirty();
    assert!(
        !order.offer_ready(),
        "a later lost notice needs another complete proof"
    );
}

#[test]
fn g1a_uncertain_handback_failure_dirty_race_preserves_cause_and_whole_retry_range() {
    let channel = 6_325_774;
    let original = order(channel);
    let overflow = original.pending_overflow();
    let snapshot = original.pending_snapshot();
    let closing = Closing::frozen_for_test(ProviderKind::Claude, channel);
    closing.begin_handback().unwrap();
    closing
        .release_after_handback_with_pending(&snapshot)
        .unwrap();
    let cap = handback_scan(channel, vec![11, 12], 12, true);
    let result = handback_commit_from_scan(cap, &[11, 12], || {
        overflow.mark_dirty();
        Err::<(), _>(Failure::Persistence)
    });
    assert!(matches!(result, Err(Failure::Persistence)));
    assert!(modes::order_barrier(&ProviderKind::Claude, channel));
    let ticket = handback_fetch_ticket(&ProviderKind::Claude, channel).unwrap();
    assert!(handback_begin_scan(ticket, vec![12], 12, true).is_err());
    let cap = handback_scan(channel, vec![11, 12], 12, true);
    assert_eq!(cap.ticket.stamp.frontier, 0);
    let (_, cap) = handback_commit_from_scan(cap, &[11, 12], || Ok(())).unwrap();
    assert!(handback_complete(cap).is_err());
    let cap = handback_scan(channel, vec![], 12, true);
    assert_eq!(
        modes::settle_order_barrier(handback_complete(cap).unwrap()),
        Ok(true)
    );
}

const EXTERNAL: u64 = crate::services::tui_input::input_key::EXTERNAL_KEY_BASE + 5;
const VOICE: u64 = 9_000_000_000_000_000_000;

#[test]
fn pending_entry_rejects_mixed_without_partial_insert() {
    let mut order = order(6_325_790);
    for sources in [
        vec![11, EXTERNAL],
        vec![EXTERNAL, 11],
        vec![11, VOICE],
        vec![11, 0],
    ] {
        assert_eq!(
            order.admit_pending(&sources).err(),
            Some(Failure::StalePermit)
        );
        let snapshot = order.pending_snapshot();
        assert!(snapshot.sources.is_empty(), "{sources:?}");
        assert_eq!(snapshot.dirty_generation, 0, "{sources:?}");
    }
    assert_eq!(order.admit_pending(&[11, 12]).unwrap().sources, [11, 12]);
}

#[test]
fn scan_namespace_rejection_preserves_frontier() {
    let mut order = order(6_325_791);
    for (sources, horizon) in [
        (vec![11, EXTERNAL], EXTERNAL),
        (vec![11], EXTERNAL),
        (vec![], VOICE),
    ] {
        assert_eq!(
            begin(&mut order, 7, sources, horizon, true).err(),
            Some(Failure::StalePermit)
        );
        assert_eq!(order.stamp.frontier, 10);
    }
    let mut cap = scan(&mut order, 7, vec![11], 11, true);
    order.settle(&mut cap, 7, 11).unwrap();
    assert_eq!(order.stamp.frontier, 11);
}

#[test]
fn handback_snapshot_rejects_before_release() {
    let channel = 6_325_792;
    let releases = std::cell::Cell::new(0);
    let snapshot = PendingSource {
        sources: vec![11, EXTERNAL],
        dirty_generation: 3,
        overflow: PendingOverflow::new(ProviderKind::Claude, channel),
    };
    let result = install_handback_snapshot(ProviderKind::Claude, channel, &snapshot, || {
        releases.set(releases.get() + 1);
        Ok(8)
    });
    assert_eq!(result, Err(Failure::StalePermit));
    assert_eq!(releases.get(), 0);
    assert!(!modes::order_barrier(&ProviderKind::Claude, channel));
    // A refused live notice leaves a settled post-handback barrier settled.
    release(channel, &[], 9);
    let cap = handback_scan(channel, vec![], 11, true);
    assert_eq!(
        modes::settle_order_barrier(handback_complete(cap).unwrap()),
        Ok(true)
    );
    assert_eq!(
        handback_pending(&ProviderKind::Claude, channel, &[11, EXTERNAL]).err(),
        Some(Failure::StalePermit)
    );
    assert!(!modes::order_barrier(&ProviderKind::Claude, channel));
}
