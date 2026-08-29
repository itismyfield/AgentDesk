use std::sync::{Arc, Barrier};

use poise::serenity_prelude::ChannelId;

use super::*;

fn key(channel: u64, generation: u64, message: u64) -> DeliveryLeaseKey {
    DeliveryLeaseKey::new(ChannelId::new(channel), generation, message, None, None)
}

fn owned_key(channel: u64, started_at: &str) -> DeliveryLeaseKey {
    DeliveryLeaseKey::new(ChannelId::new(channel), 7, 0, Some(started_at), Some(41))
}

fn coordinate(value: u64) -> ConsumedCoordinate {
    ConsumedCoordinate::new(value).expect("test coordinates are positive")
}

fn cell(channel: u64) -> Arc<DeliveryLeaseCell> {
    Arc::new(DeliveryLeaseCell::new(ChannelId::new(channel)))
}

fn snapshot(cell: &DeliveryLeaseCell) -> String {
    format!("{:?}", cell.read())
}

fn assert_unleased(cell: &DeliveryLeaseCell) {
    assert!(matches!(cell.read(), super::super::LeaseSnapshot::Unleased));
}

#[test]
fn successful_transitions_drop_predecessors_and_keep_owned_key() {
    let cell = cell(51_911);
    let started_at = String::from("owned-started-at");
    let key = owned_key(51_911, &started_at);
    let acquired = PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(41), 900)
        .expect("first acquisition wins");
    drop(started_at);
    let owned_key_debug = format!("{key:?}");
    assert!(owned_key_debug.contains("owned-started-at"));
    assert_eq!(Arc::strong_count(&cell), 2);
    let before_renew = super::super::payload_lock_entries_for_test();
    assert!(acquired.renew(1_200));
    assert_eq!(
        super::super::payload_lock_entries_for_test() - before_renew,
        1
    );

    let acquired_key = acquired.key.clone();
    let before_commit = super::super::payload_lock_entries_for_test();
    let committed = acquired
        .commit(LeaseOutcome::Delivered)
        .unwrap_or_else(|_| panic!("matching commit must succeed"));
    assert_eq!(
        super::super::payload_lock_entries_for_test() - before_commit,
        1
    );
    assert_eq!(Arc::strong_count(&cell), 2, "acquired predecessor leaked");
    assert_eq!(
        acquired_key, committed.key,
        "commit must retain the owned started_at key"
    );
    assert!(matches!(
        cell.read(),
        super::super::LeaseSnapshot::Committed { key: current, start: 41, end: 41, .. }
            if current == key
    ));
    let committed_key = committed.key.clone();
    let before_release = super::super::payload_lock_entries_for_test();
    let released = committed
        .release()
        .unwrap_or_else(|_| panic!("matching release must succeed"));
    assert_eq!(
        super::super::payload_lock_entries_for_test() - before_release,
        1
    );
    assert_eq!(Arc::strong_count(&cell), 2, "committed predecessor leaked");
    assert_eq!(
        committed_key, released.key,
        "release must retain the owned started_at key"
    );
    assert_unleased(&cell);
    drop(released);
    assert_eq!(Arc::strong_count(&cell), 1);

    assert!(cell.try_acquire(key, LeaseHolder::Sink, 41, 42, 2_000));
    assert!(cell.release(
        LeaseHolder::Sink,
        owned_key(51_911, "owned-started-at"),
        41,
        42
    ));
}

#[test]
fn legacy_apis_cannot_mutate_exact_acquired_or_committed_state() {
    let cell = cell(51_920);
    let key = key(51_920, 2, 3);
    let acquired = PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(70), 10)
        .expect("exact acquire");
    let acquired_before = snapshot(&cell);
    assert!(!cell.try_acquire(key.clone(), LeaseHolder::Sink, 70, 70, 500));
    assert!(!cell.renew(LeaseHolder::Sink, key.clone(), 999));
    assert!(!cell.commit(
        LeaseHolder::Sink,
        key.clone(),
        70,
        70,
        LeaseOutcome::Delivered,
    ));
    assert!(!cell.release(LeaseHolder::Sink, key.clone(), 70, 70));
    assert!(!cell.reclaim_if_expired(10));
    assert_eq!(snapshot(&cell), acquired_before);

    let committed = acquired
        .commit(LeaseOutcome::Delivered)
        .unwrap_or_else(|_| panic!("exact commit"));
    let committed_before = snapshot(&cell);
    assert!(!cell.release(LeaseHolder::Sink, key, 70, 70));
    assert_eq!(snapshot(&cell), committed_before);
    drop(committed);
    assert_unleased(&cell);
}

#[test]
fn exact_reclaim_validates_every_field_and_deadline_without_mutation() {
    let owner = cell(51_930);
    let other = cell(51_930);
    let lease_key = key(51_930, 5, 6);
    let wrong_key = key(51_930, 5, 7);
    let acquired =
        PinnedLeaseCell::acquire(Arc::clone(&owner), lease_key.clone(), coordinate(90), 100)
            .expect("exact acquire");
    let before = snapshot(&owner);
    let cases = [
        (&other, &lease_key, acquired.token, coordinate(90), 100),
        (&owner, &wrong_key, acquired.token, coordinate(90), 100),
        (&owner, &lease_key, LeaseToken::next(), coordinate(90), 100),
        (&owner, &lease_key, acquired.token, coordinate(91), 100),
        (&owner, &lease_key, acquired.token, coordinate(90), 99),
    ];
    for (pinned, candidate_key, token, candidate_c, now) in cases {
        assert!(!owner.reclaim_exact_if_expired(pinned, candidate_key, token, candidate_c, now,));
        assert_eq!(snapshot(&owner), before);
    }
    assert!(acquired.reclaim_if_expired(100));
    assert_unleased(&owner);
    drop(acquired);
}

#[test]
fn stale_exact_and_legacy_calls_cannot_mutate_same_identity_replacement() {
    let cell = cell(51_940);
    let key = key(51_940, 8, 9);
    let stale = PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(100), 10)
        .expect("stale acquire");
    let stale_token = stale.token;
    assert!(stale.reclaim_if_expired(10));
    let replacement =
        PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(100), 1_000)
            .expect("same-identity replacement");
    let replacement_token = replacement.token;
    assert_ne!(stale_token, replacement_token, "tokens must not be reused");
    let before = snapshot(&cell);
    assert!(!stale.matches());
    assert!(!cell.renew(LeaseHolder::Sink, key.clone(), 2_000));
    assert!(!cell.commit(
        LeaseHolder::Sink,
        key.clone(),
        100,
        100,
        LeaseOutcome::Delivered,
    ));
    assert!(!cell.release(LeaseHolder::Sink, key.clone(), 100, 100));
    assert!(!cell.reclaim_if_expired(u64::MAX));
    drop(stale);
    assert_eq!(snapshot(&cell), before);
    assert_eq!(replacement.token, replacement_token);
    assert!(replacement.matches());
    drop(replacement);
    assert_unleased(&cell);

    let acquired = PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(100), 20)
        .expect("committed stale acquire");
    let committed = acquired
        .commit(LeaseOutcome::Delivered)
        .unwrap_or_else(|_| panic!("committed stale commit"));
    assert!(committed.release_owned_state());
    let successor = PinnedLeaseCell::acquire(cell, key, coordinate(100), 2_000)
        .expect("successor after committed release");
    drop(committed);
    assert!(
        successor.matches(),
        "committed stale drop released successor"
    );
}

#[test]
fn barrier_orders_stale_legacy_renew_after_exact_reclaim_and_replacement() {
    let cell = cell(51_950);
    let key = key(51_950, 11, 12);
    let stale = PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(110), 10)
        .expect("stale acquire");
    let barrier = Arc::new(Barrier::new(2));
    let thread_cell = Arc::clone(&cell);
    let thread_key = key.clone();
    let thread_barrier = Arc::clone(&barrier);
    let renewer = std::thread::spawn(move || {
        thread_barrier.wait();
        thread_cell.renew(LeaseHolder::Sink, thread_key, 9_999)
    });
    assert!(stale.reclaim_if_expired(10));
    let replacement = PinnedLeaseCell::acquire(Arc::clone(&cell), key, coordinate(110), 500)
        .expect("replacement acquire");
    let before = snapshot(&cell);
    barrier.wait();
    assert!(!renewer.join().expect("renewer thread"));
    assert_eq!(snapshot(&cell), before);
    assert!(replacement.matches());
}

#[test]
fn exact_identity_mismatch_drop_and_single_winner_regressions() {
    let other = cell(51_960);
    let cell = cell(51_960);
    let lease_key = key(51_960, 13, 14);
    let acquired =
        PinnedLeaseCell::acquire(Arc::clone(&cell), lease_key.clone(), coordinate(120), 500)
            .expect("acquire");
    let wrong_key = key(51_960, 13, 15);
    let wrong_token = LeaseToken::next();
    let before = snapshot(&cell);
    let mismatches = [
        (&other, &lease_key, acquired.token, coordinate(120)),
        (&cell, &wrong_key, acquired.token, coordinate(120)),
        (&cell, &lease_key, wrong_token, coordinate(120)),
        (&cell, &lease_key, acquired.token, coordinate(121)),
    ];
    for (pinned, candidate_key, token, candidate_c) in mismatches {
        assert!(!cell.commit_exact(
            pinned,
            candidate_key,
            token,
            candidate_c,
            LeaseOutcome::Delivered,
        ));
        assert!(!cell.release_exact(pinned, candidate_key, token, candidate_c));
        assert_eq!(snapshot(&cell), before);
    }
    assert!(acquired.matches());
    drop(acquired);
    assert_unleased(&cell);

    let barrier = Arc::new(Barrier::new(9));
    let mut threads = Vec::new();
    for _ in 0..8 {
        let cell = Arc::clone(&cell);
        let lease_key = lease_key.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            PinnedLeaseCell::acquire(cell, lease_key, coordinate(120), 1_000)
        }));
    }
    barrier.wait();
    let winners: Vec<_> = threads
        .into_iter()
        .filter_map(|thread| thread.join().expect("acquirer thread"))
        .collect();
    assert_eq!(winners.len(), 1);
    assert!(winners[0].matches());
}
