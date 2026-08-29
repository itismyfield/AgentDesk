use std::sync::{Arc, Barrier};

use poise::serenity_prelude::ChannelId;

use super::*;

fn key(channel: u64, generation: u64, message: u64) -> DeliveryLeaseKey {
    DeliveryLeaseKey::new(ChannelId::new(channel), generation, message, None, None)
}

fn coordinate(value: u64) -> ConsumedCoordinate {
    ConsumedCoordinate::new(value).expect("test coordinates are positive")
}

fn cell(channel: u64) -> Arc<DeliveryLeaseCell> {
    Arc::new(DeliveryLeaseCell::new(ChannelId::new(channel)))
}

fn unleased(cell: &DeliveryLeaseCell) -> bool {
    matches!(cell.read(), super::super::LeaseSnapshot::Unleased)
}

fn committed(cell: &DeliveryLeaseCell, expected_key: &DeliveryLeaseKey, expected_c: u64) -> bool {
    matches!(
        cell.read(),
        super::super::LeaseSnapshot::Committed {
            key,
            start,
            end,
            outcome: LeaseOutcome::Delivered,
            ..
        } if key == *expected_key && start == expected_c && end == expected_c
    )
}

#[test]
fn matching_exact_identity_commits_and_typed_release_clears() {
    let cell = cell(51_911);
    let key = key(51_911, 7, 80);
    let acquired = PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(41), 900)
        .expect("first acquisition wins");
    assert!(acquired.matches());
    assert!(acquired.renew(1_200));

    let committed_guard = match acquired.commit(LeaseOutcome::Delivered) {
        Ok(guard) => guard,
        Err(_) => panic!("matching commit must succeed"),
    };
    assert!(committed(&cell, &key, 41));
    let _released = match committed_guard.release() {
        Ok(guard) => guard,
        Err(_) => panic!("matching committed release must succeed"),
    };
    assert!(unleased(&cell));
}

#[test]
fn every_exact_identity_mismatch_is_zero_mutation() {
    let cases = ["key", "token", "coordinate", "cell"];
    for mismatch in cases {
        let owner = cell(51_920);
        let other = cell(51_920);
        let key = key(51_920, 2, 3);
        let wrong_key = DeliveryLeaseKey::new(ChannelId::new(51_920), 2, 4, None, None);
        let held = PinnedLeaseCell::acquire(Arc::clone(&owner), key.clone(), coordinate(70), 500)
            .expect("acquire");
        let wrong_token = LeaseToken::next();

        let (receiver, pinned, candidate_key, candidate_token, candidate_c) = match mismatch {
            "key" => (&owner, &owner, &wrong_key, held.token, coordinate(70)),
            "token" => (&owner, &owner, &key, wrong_token, coordinate(70)),
            "coordinate" => (&owner, &owner, &key, held.token, coordinate(71)),
            // Invoke the owner cell with a different pinned Arc. Pointer identity,
            // not channel or key equality, must reject this candidate.
            "cell" => (&owner, &other, &key, held.token, coordinate(70)),
            _ => unreachable!(),
        };

        assert!(
            !receiver.commit_exact(
                pinned,
                candidate_key,
                candidate_token,
                candidate_c,
                LeaseOutcome::Delivered,
            ),
            "{mismatch} mismatch must reject commit"
        );
        assert!(
            !receiver.release_exact(pinned, candidate_key, candidate_token, candidate_c,),
            "{mismatch} mismatch must reject release"
        );
        assert!(held.matches(), "{mismatch} mismatch mutated the live lease");
        drop(held);
        assert!(unleased(&owner));
        assert!(unleased(&other));
    }
}

#[test]
fn transport_error_drop_releases_only_the_exact_acquisition() {
    let cell = cell(51_930);
    let key = key(51_930, 5, 6);
    {
        let guard = PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(90), 500)
            .expect("acquire before transport");
        assert!(guard.matches());
        // Simulate transport returning early: the acquired guard is dropped.
    }
    assert!(unleased(&cell));

    let replacement = PinnedLeaseCell::acquire(Arc::clone(&cell), key, coordinate(90), 600)
        .expect("drop made only its exact acquisition available");
    assert!(replacement.matches());
}

#[test]
fn stale_pre_reset_guard_cannot_drop_replacement() {
    let cell = cell(51_940);
    let key = key(51_940, 8, 9);
    let stale = PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(100), 10)
        .expect("stale acquire");
    assert!(cell.reclaim_if_expired(10));
    let replacement = PinnedLeaseCell::acquire(Arc::clone(&cell), key, coordinate(100), 1_000)
        .expect("replacement acquire");

    drop(stale);
    assert!(replacement.matches(), "stale drop released replacement");
    drop(replacement);
    assert!(unleased(&cell));
}

#[test]
fn reacquire_same_key_and_coordinate_gets_new_token() {
    let cell = cell(51_950);
    let key = key(51_950, 11, 12);
    let first = PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(110), 10)
        .expect("first acquire");
    let first_token = first.token;
    assert!(cell.reclaim_if_expired(10));
    let second = PinnedLeaseCell::acquire(Arc::clone(&cell), key, coordinate(110), 500)
        .expect("second acquire");

    assert_ne!(first_token, second.token, "tokens must never be reused");
    assert!(!first.matches());
    assert!(second.matches());
}

#[test]
fn concurrent_acquire_has_one_winner() {
    let cell = cell(51_960);
    let key = key(51_960, 13, 14);
    let barrier = Arc::new(Barrier::new(9));
    let mut threads = Vec::new();
    for _ in 0..8 {
        let cell = Arc::clone(&cell);
        let key = key.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            PinnedLeaseCell::acquire(cell, key, coordinate(120), 1_000)
        }));
    }
    barrier.wait();
    let guards: Vec<_> = threads
        .into_iter()
        .filter_map(|thread| thread.join().expect("acquirer thread"))
        .collect();
    assert_eq!(guards.len(), 1, "exactly one concurrent acquire wins");
    assert!(guards[0].matches());
}

#[test]
fn renew_commit_and_release_hold_one_payload_lock() {
    let cell = cell(51_965);
    let key = key(51_965, 14, 15);
    let acquired = PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(125), 10)
        .expect("acquire");

    let before_renew = super::super::payload_lock_entries_for_test();
    assert!(acquired.renew(20));
    assert_eq!(
        super::super::payload_lock_entries_for_test() - before_renew,
        1,
        "exact renew must check and mutate under one payload lock"
    );

    let before_commit = super::super::payload_lock_entries_for_test();
    let committed = match acquired.commit(LeaseOutcome::Delivered) {
        Ok(guard) => guard,
        Err(_) => panic!("matching commit must succeed"),
    };
    assert_eq!(
        super::super::payload_lock_entries_for_test() - before_commit,
        1,
        "exact commit must check and mutate under one payload lock"
    );

    let before_release = super::super::payload_lock_entries_for_test();
    let _released = match committed.release() {
        Ok(guard) => guard,
        Err(_) => panic!("matching release must succeed"),
    };
    assert_eq!(
        super::super::payload_lock_entries_for_test() - before_release,
        1,
        "exact release must check and mutate under one payload lock"
    );
}

#[test]
fn committed_drop_releases_only_its_own_exact_state() {
    let cell = cell(51_970);
    let key = key(51_970, 15, 16);
    let acquired = PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(130), 10)
        .expect("acquire");
    let committed_guard = match acquired.commit(LeaseOutcome::Delivered) {
        Ok(guard) => guard,
        Err(_) => panic!("commit result must be handled"),
    };
    assert!(committed(&cell, &key, 130));
    assert!(committed_guard.release_owned_state());
    let replacement =
        PinnedLeaseCell::acquire(Arc::clone(&cell), key.clone(), coordinate(130), 1_000)
            .expect("replacement after committed release");

    drop(committed_guard);
    assert!(
        replacement.matches(),
        "committed stale drop released replacement"
    );
    drop(replacement);
    assert!(unleased(&cell));
}
