use super::*;

fn order(channel: u64) -> AdmissionOrder {
    AdmissionOrder::new(ProviderKind::Claude, channel, 7, 10)
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
        assert!(order.begin_scan(7, sources, horizon, true).is_err());
    }
    let mut cap = order.begin_scan(7, vec![11, 12], 12, true).unwrap();
    assert_eq!(order.permits(&cap, 7, 12), Err(Failure::StalePermit));
    order.settle(&mut cap, 7, 11).unwrap();
    assert_eq!(order.frontier, 11);
    assert_eq!(order.permits(&cap, 7, 12), Ok(()));
    order.settle(&mut cap, 7, 12).unwrap();
    assert!(order.complete(cap, 7).is_ok());
    assert!(order.pending(&[11, 12]).sources.is_empty());
    assert_eq!(order.dirty, 1);
}

#[test]
fn g1a_deferred_head_cannot_be_overtaken_in_this_or_a_later_scan() {
    let mut order = order(6_325_711);
    let cap = order.begin_scan(7, vec![11, 12], 12, true).unwrap();
    order.defer(&cap, 7, 11).unwrap();
    assert_eq!(order.permits(&cap, 7, 12), Err(Failure::StalePermit));
    assert!(order.begin_scan(7, vec![12], 12, true).is_err());
    let mut retry = order.begin_scan(7, vec![11, 12], 12, true).unwrap();
    order.settle(&mut retry, 7, 11).unwrap();
    order.settle(&mut retry, 7, 12).unwrap();
    assert!(order.complete(retry, 7).is_ok());
}

#[test]
fn g1a_an_older_deferred_head_does_not_erase_the_previous_retry_obligation() {
    let mut order = AdmissionOrder::new(ProviderKind::Claude, 6_325_719, 7, 0);
    let cap = order.begin_scan(7, vec![20, 30], 30, true).unwrap();
    order.defer(&cap, 7, 20).unwrap();
    let older = order.begin_scan(7, vec![10, 20], 20, true).unwrap();
    order.defer(&older, 7, 10).unwrap();
    let mut retry = order.begin_scan(7, vec![10], 10, true).unwrap();
    order.settle(&mut retry, 7, 10).unwrap();
    assert!(order.begin_scan(7, vec![30], 30, true).is_err());
    let mut retry = order.begin_scan(7, vec![20, 30], 30, true).unwrap();
    order.settle(&mut retry, 7, 20).unwrap();
    order.settle(&mut retry, 7, 30).unwrap();
    assert!(order.complete(retry, 7).is_ok());
}

#[test]
fn g1a_epoch_dirty_scan_and_scope_changes_refuse_stale_commits() {
    let mut order = order(6_325_712);
    let cap = order.begin_scan(7, vec![11], 11, true).unwrap();
    assert_eq!(order.permits(&cap, 8, 11), Err(Failure::StalePermit));
    let mut other = AdmissionOrder::new(ProviderKind::Claude, 6_325_712, 7, 10);
    let _other_cap = other.begin_scan(7, vec![11], 11, true).unwrap();
    assert_eq!(other.permits(&cap, 7, 11), Err(Failure::StalePermit));
    order.pending(&[12]);
    assert_eq!(order.permits(&cap, 7, 11), Err(Failure::StalePermit));
    let cap = order.begin_scan(7, vec![11, 12], 12, true).unwrap();
    let fresh = order.begin_scan(7, vec![11, 12], 12, true).unwrap();
    assert_eq!(order.permits(&cap, 7, 11), Err(Failure::StalePermit));
    order.invalidate(8);
    assert_eq!(order.permits(&fresh, 8, 11), Err(Failure::StalePermit));
    assert!(order.begin_scan(7, vec![11, 12], 12, true).is_err());
    let cap = order.begin_scan(8, vec![11, 12], 12, true).unwrap();
    assert_eq!(order.permits(&cap, 8, 11), Ok(()));
}

#[test]
fn g1a_capability_checks_provider_channel_owner_and_named_epoch() {
    let mut order = order(6_325_717);
    for field in 0..4 {
        let mut cap = order.begin_scan(7, vec![11], 11, true).unwrap();
        match field {
            0 => cap.provider = ProviderKind::Codex,
            1 => cap.channel += 1,
            2 => cap.owner += 1,
            _ => cap.epoch += 1,
        }
        assert_eq!(order.permits(&cap, 7, 11), Err(Failure::StalePermit));
    }
}

#[test]
fn g1a_complete_requires_full_horizon_all_pending_and_no_unsettled_page() {
    let mut order = order(6_325_713);
    order.pending(&[12]);
    let mut cap = order.begin_scan(7, vec![11], 11, true).unwrap();
    order.settle(&mut cap, 7, 11).unwrap();
    assert!(order.complete(cap, 7).is_err());
    let cap = order.begin_scan(7, vec![12], 12, true).unwrap();
    assert!(order.complete(cap, 7).is_err());
    let mut cap = order.begin_scan(7, vec![12], 12, false).unwrap();
    order.settle(&mut cap, 7, 12).unwrap();
    assert!(order.complete(cap, 7).is_err());
    let cap = order.begin_scan(7, vec![], 12, true).unwrap();
    order.pending(&[13]);
    assert!(order.complete(cap, 7).is_err());
    let mut cap = order.begin_scan(7, vec![13], 13, true).unwrap();
    order.settle(&mut cap, 7, 13).unwrap();
    assert!(order.complete(cap, 7).is_ok());
}

#[test]
fn g1a_handback_requires_complete_fetch_after_known_pending_settles() {
    let channel = 6_325_718;
    let closing = super::super::fence::Closing::frozen_for_test(ProviderKind::Claude, channel);
    closing.begin_handback().unwrap();
    closing.release_after_handback().unwrap();
    let mut order = order(channel);
    order.pending(&[11]);
    let mut cap = order.begin_scan(7, vec![11], 11, true).unwrap();
    order.settle(&mut cap, 7, 11).unwrap();
    let before = order.complete(cap, 7).unwrap();
    assert_eq!(
        modes::settle_order_barrier(&order, before, 7),
        Err(Failure::StalePermit)
    );
    assert!(modes::order_barrier(&ProviderKind::Claude, channel));
    let cap = order.begin_scan(7, vec![], 11, true).unwrap();
    let after = order.complete(cap, 7).unwrap();
    assert_eq!(modes::settle_order_barrier(&order, after, 7), Ok(true));
}
