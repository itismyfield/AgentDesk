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
    ConsumedCoordinate::new(value).expect("positive test coordinate")
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
fn channel_authority_is_coupled_before_acquire_mutation() {
    assert!(ConsumedCoordinate::new(0).is_none());
    let cell_a = cell(51_910);
    let key_b = key(51_911, 1, 2);
    assert!(
        PinnedLeaseCell::acquire(Arc::clone(&cell_a), key_b.clone(), coordinate(1), 10).is_none()
    );
    assert_unleased(&cell_a);

    let cell_b = cell(51_911);
    let canonical = PinnedLeaseCell::acquire(cell_b, key_b, coordinate(1), 10)
        .expect("canonical channel cell acquires");
    assert!(canonical.matches());
}

#[test]
fn transitions_are_single_lock_raii_and_keep_owned_identity() {
    let cell = cell(51_920);
    let started_at = String::from("owned-start");
    let lease_key = owned_key(51_920, &started_at);
    let acquired =
        PinnedLeaseCell::acquire(Arc::clone(&cell), lease_key.clone(), coordinate(41), 100)
            .expect("acquire");
    drop(started_at);
    assert_eq!(Arc::strong_count(&cell), 2);

    let before = super::super::payload_lock_entries_for_test();
    assert!(acquired.renew(200));
    assert_eq!(super::super::payload_lock_entries_for_test() - before, 1);
    let before = super::super::payload_lock_entries_for_test();
    let committed = acquired
        .commit(LeaseOutcome::Delivered)
        .unwrap_or_else(|_| panic!("commit"));
    assert_eq!(super::super::payload_lock_entries_for_test() - before, 1);
    assert_eq!(Arc::strong_count(&cell), 2, "acquired predecessor leaked");
    assert_eq!(committed.key, lease_key);
    assert!(matches!(
        cell.read(),
        super::super::LeaseSnapshot::Committed {
            holder: LeaseHolder::Sink,
            outcome: LeaseOutcome::Delivered,
            start: 41,
            end: 41,
            ..
        }
    ));
    let before = super::super::payload_lock_entries_for_test();
    let released = committed.release().unwrap_or_else(|_| panic!("release"));
    assert_eq!(super::super::payload_lock_entries_for_test() - before, 1);
    assert_eq!(Arc::strong_count(&cell), 2, "committed predecessor leaked");
    drop(released);
    assert_eq!(Arc::strong_count(&cell), 1);
    assert_unleased(&cell);

    let transport_error =
        PinnedLeaseCell::acquire(Arc::clone(&cell), lease_key, coordinate(41), 300)
            .expect("transport attempt");
    drop(transport_error);
    assert_unleased(&cell);
}

#[test]
fn exact_and_legacy_acquire_have_one_concurrent_winner() {
    enum Result {
        Exact(PinnedLeaseCell<Acquired>),
        Legacy,
        Lost,
    }

    let cell = cell(51_930);
    let lease_key = key(51_930, 2, 3);
    let barrier = Arc::new(Barrier::new(9));
    let mut threads = Vec::new();
    for index in 0..8 {
        let cell = Arc::clone(&cell);
        let lease_key = lease_key.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            if index % 2 == 0 {
                PinnedLeaseCell::acquire(cell, lease_key, coordinate(50), 100)
                    .map_or(Result::Lost, Result::Exact)
            } else if cell.try_acquire(lease_key, LeaseHolder::Sink, 50, 50, 100) {
                Result::Legacy
            } else {
                Result::Lost
            }
        }));
    }
    barrier.wait();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().expect("acquirer"))
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Result::Exact(_) | Result::Legacy))
            .count(),
        1
    );
    let exact_won = results
        .iter()
        .any(|result| matches!(result, Result::Exact(_)));
    assert!(results.iter().all(|result| match result {
        Result::Exact(guard) => guard.matches(),
        Result::Legacy | Result::Lost => true,
    }));
    drop(results);
    if !exact_won {
        assert!(cell.release(LeaseHolder::Sink, lease_key, 50, 50));
    }
    assert_unleased(&cell);
}

fn assert_identity_mismatches(
    mut transition: impl FnMut(
        &DeliveryLeaseCell,
        &Arc<DeliveryLeaseCell>,
        &DeliveryLeaseKey,
        LeaseToken,
        ConsumedCoordinate,
    ) -> bool,
) {
    let owner = cell(51_940);
    let other = cell(51_940);
    let lease_key = owned_key(51_940, "identity-a");
    let wrong_key = owned_key(51_940, "identity-b");
    let acquired =
        PinnedLeaseCell::acquire(Arc::clone(&owner), lease_key.clone(), coordinate(60), 100)
            .expect("acquire");
    let before = snapshot(&owner);
    for (pinned, candidate_key, token, c) in [
        (&other, &lease_key, acquired.token, coordinate(60)),
        (&owner, &wrong_key, acquired.token, coordinate(60)),
        (&owner, &lease_key, LeaseToken::next(), coordinate(60)),
        (&owner, &lease_key, acquired.token, coordinate(61)),
    ] {
        assert!(!transition(&owner, pinned, candidate_key, token, c));
        assert_eq!(snapshot(&owner), before);
    }
}

#[test]
fn renew_requires_every_exact_identity_field() {
    assert_identity_mismatches(|owner, pinned, key, token, c| {
        owner.renew_exact(pinned, key, token, c, 999)
    });
}

#[test]
fn commit_requires_every_exact_identity_field() {
    assert_identity_mismatches(|owner, pinned, key, token, c| {
        owner.commit_exact(pinned, key, token, c, LeaseOutcome::Delivered)
    });
}

#[test]
fn release_requires_every_exact_identity_field() {
    assert_identity_mismatches(|owner, pinned, key, token, c| {
        owner.release_exact(pinned, key, token, c, |_| true)
    });
}

#[test]
fn reclaim_requires_every_exact_identity_field_and_deadline() {
    assert_identity_mismatches(|owner, pinned, key, token, c| {
        owner.reclaim_exact_if_expired(pinned, key, token, c, 100)
    });
    let owner = cell(51_941);
    let acquired =
        PinnedLeaseCell::acquire(Arc::clone(&owner), key(51_941, 4, 5), coordinate(60), 100)
            .expect("acquire");
    let before = snapshot(&owner);
    assert!(!acquired.reclaim_if_expired(99));
    assert_eq!(snapshot(&owner), before);
    assert!(acquired.reclaim_if_expired(100));
    assert_unleased(&owner);
}

#[test]
fn legacy_surface_cannot_mutate_exact_state_or_replacement() {
    let cell = cell(51_950);
    let lease_key = key(51_950, 7, 8);
    let acquired =
        PinnedLeaseCell::acquire(Arc::clone(&cell), lease_key.clone(), coordinate(70), 10)
            .expect("acquire");
    let before = snapshot(&cell);
    assert!(!cell.try_acquire(lease_key.clone(), LeaseHolder::Sink, 70, 70, 20));
    assert!(!cell.renew(LeaseHolder::Sink, lease_key.clone(), 20));
    assert!(!cell.commit(
        LeaseHolder::Sink,
        lease_key.clone(),
        70,
        70,
        LeaseOutcome::Delivered
    ));
    assert!(!cell.release(LeaseHolder::Sink, lease_key.clone(), 70, 70));
    assert!(!cell.reclaim_if_expired(10));
    assert_eq!(snapshot(&cell), before);

    let stale_token = acquired.token;
    assert!(acquired.reclaim_if_expired(10));
    let replacement =
        PinnedLeaseCell::acquire(Arc::clone(&cell), lease_key.clone(), coordinate(70), 100)
            .expect("replacement");
    assert_ne!(
        stale_token, replacement.token,
        "tokens must never be reused"
    );
    let stale = match acquired.commit(LeaseOutcome::Delivered) {
        Err(stale) => stale,
        Ok(_) => panic!("reclaimed predecessor committed replacement"),
    };
    drop(stale);
    assert!(
        replacement.matches(),
        "stale acquired drop released replacement"
    );

    let replacement_before = snapshot(&cell);
    assert!(!cell.try_acquire(lease_key.clone(), LeaseHolder::Sink, 70, 70, 999));
    assert!(!cell.renew(LeaseHolder::Sink, lease_key.clone(), 999));
    assert!(!cell.commit(
        LeaseHolder::Sink,
        lease_key.clone(),
        70,
        70,
        LeaseOutcome::Delivered
    ));
    assert!(!cell.release(LeaseHolder::Sink, lease_key.clone(), 70, 70));
    assert!(!cell.reclaim_if_expired(u64::MAX));
    assert_eq!(snapshot(&cell), replacement_before);
    drop(replacement);

    let acquired =
        PinnedLeaseCell::acquire(Arc::clone(&cell), lease_key.clone(), coordinate(70), 200)
            .expect("committed stale acquire");
    let committed = acquired
        .commit(LeaseOutcome::Delivered)
        .unwrap_or_else(|_| panic!("commit"));
    let committed_before = snapshot(&cell);
    assert!(!cell.try_acquire(lease_key.clone(), LeaseHolder::Sink, 70, 70, 999));
    assert!(!cell.renew(LeaseHolder::Sink, lease_key.clone(), 999));
    assert!(!cell.commit(
        LeaseHolder::Sink,
        lease_key.clone(),
        70,
        70,
        LeaseOutcome::Unknown
    ));
    assert!(!cell.release(LeaseHolder::Sink, lease_key.clone(), 70, 70));
    assert!(!cell.reclaim_if_expired(u64::MAX));
    assert_eq!(snapshot(&cell), committed_before);

    assert!(committed.release_owned_state());
    let successor =
        PinnedLeaseCell::acquire(cell, lease_key, coordinate(70), 300).expect("successor");
    drop(committed);
    assert!(
        successor.matches(),
        "stale committed drop released successor"
    );
}

#[test]
fn ordered_stale_legacy_heartbeat_cannot_renew_replacement() {
    let cell = cell(51_960);
    let lease_key = key(51_960, 9, 10);
    let stale = PinnedLeaseCell::acquire(Arc::clone(&cell), lease_key.clone(), coordinate(80), 10)
        .expect("stale");
    let barrier = Arc::new(Barrier::new(2));
    let thread_cell = Arc::clone(&cell);
    let thread_key = lease_key.clone();
    let thread_barrier = Arc::clone(&barrier);
    let heartbeat = std::thread::spawn(move || {
        thread_barrier.wait();
        thread_cell.renew(LeaseHolder::Sink, thread_key, 9_999)
    });
    assert!(stale.reclaim_if_expired(10));
    let replacement =
        PinnedLeaseCell::acquire(cell, lease_key, coordinate(80), 500).expect("replacement");
    barrier.wait();
    assert!(!heartbeat.join().expect("heartbeat"));
    assert!(replacement.matches());
}
