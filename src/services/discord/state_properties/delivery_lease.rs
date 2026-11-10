//! Delivery-lease cell: proptest state machine plus contended workers.
//!
//! The cell's mutex is `std::sync::Mutex`, which loom cannot interleave without
//! changing production synchronization. Contended coverage uses OS threads.

use std::sync::Arc;
use std::thread;

use proptest::prelude::*;
use proptest::strategy::BoxedStrategy;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest};
use serenity::model::id::ChannelId;

use super::fresh_channel;
use crate::services::discord::{
    DeliveryLeaseCell, DeliveryLeaseKey, LeaseHolder, LeaseOutcome, LeaseSnapshot,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Holder(u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key(u16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Delivered,
    NotDelivered,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Model {
    Unleased,
    Leased {
        holder: Holder,
        key: Key,
        start: u32,
        end: u32,
        deadline: u64,
    },
    Committed {
        holder: Holder,
        key: Key,
        start: u32,
        end: u32,
        outcome: Outcome,
    },
}

#[derive(Clone, Debug)]
enum Step {
    Acquire {
        holder: Holder,
        key: Key,
        start: u32,
        end: u32,
        deadline: u64,
    },
    Commit {
        holder: Holder,
        key: Key,
        start: u32,
        end: u32,
        outcome: Outcome,
    },
    Release {
        holder: Holder,
        key: Key,
        start: u32,
        end: u32,
    },
    Renew {
        holder: Holder,
        key: Key,
        deadline: u64,
    },
    Reclaim { now: u64 },
}

fn holder_of(holder: Holder) -> LeaseHolder {
    match holder.0 % 5 {
        0 => LeaseHolder::Sink,
        1 => LeaseHolder::Watcher { instance_id: 1 },
        2 => LeaseHolder::Watcher { instance_id: 2 },
        3 => LeaseHolder::Bridge { attempt_id: 1 },
        _ => LeaseHolder::OWriter { serial: 7 },
    }
}

fn outcome_of(outcome: Outcome) -> LeaseOutcome {
    match outcome {
        Outcome::Delivered => LeaseOutcome::Delivered,
        Outcome::NotDelivered => LeaseOutcome::NotDelivered,
        Outcome::Unknown => LeaseOutcome::Unknown,
    }
}

fn key_of(channel: ChannelId, key: Key) -> DeliveryLeaseKey {
    DeliveryLeaseKey::new(channel, 1, u64::from(key.0) + 1, None, None)
}

fn holder_strat() -> impl Strategy<Value = Holder> {
    (0u8..5).prop_map(Holder)
}

fn key_strat() -> impl Strategy<Value = Key> {
    (0u16..6).prop_map(Key)
}

fn outcome_strat() -> impl Strategy<Value = Outcome> {
    prop_oneof![
        Just(Outcome::Delivered),
        Just(Outcome::NotDelivered),
        Just(Outcome::Unknown),
    ]
}

fn range_strat() -> impl Strategy<Value = (u32, u32)> {
    (0u32..40, 1u32..20).prop_map(|(start, width)| (start, start.saturating_add(width)))
}

struct LeaseRef;

impl ReferenceStateMachine for LeaseRef {
    type State = Model;
    type Transition = Step;

    fn init_state() -> BoxedStrategy<Self::State> {
        Just(Model::Unleased).boxed()
    }

    fn transitions(state: &Self::State) -> BoxedStrategy<Self::Transition> {
        let _ = state;
        prop_oneof![
            (holder_strat(), key_strat(), range_strat(), 0u64..50).prop_map(
                |(holder, key, (start, end), deadline)| Step::Acquire {
                    holder,
                    key,
                    start,
                    end,
                    deadline,
                }
            ),
            (holder_strat(), key_strat(), range_strat(), outcome_strat()).prop_map(
                |(holder, key, (start, end), outcome)| Step::Commit {
                    holder,
                    key,
                    start,
                    end,
                    outcome,
                }
            ),
            (holder_strat(), key_strat(), range_strat()).prop_map(|(holder, key, (start, end))| {
                Step::Release {
                    holder,
                    key,
                    start,
                    end,
                }
            }),
            (holder_strat(), key_strat(), 0u64..80).prop_map(|(holder, key, deadline)| {
                Step::Renew {
                    holder,
                    key,
                    deadline,
                }
            }),
            (0u64..80u64).prop_map(|now| Step::Reclaim { now }),
        ]
        .boxed()
    }

    fn apply(state: Self::State, transition: &Self::Transition) -> Self::State {
        match *transition {
            Step::Acquire {
                holder,
                key,
                start,
                end,
                deadline,
            } => {
                if state == Model::Unleased && start < end {
                    Model::Leased {
                        holder,
                        key,
                        start,
                        end,
                        deadline,
                    }
                } else {
                    state
                }
            }
            Step::Commit {
                holder,
                key,
                start,
                end,
                outcome,
            } => match state {
                Model::Leased {
                    holder: held,
                    key: leased,
                    start: leased_start,
                    end: leased_end,
                    ..
                } if held == holder
                    && leased == key
                    && leased_start == start
                    && leased_end == end =>
                {
                    Model::Committed {
                        holder,
                        key,
                        start,
                        end,
                        outcome,
                    }
                }
                other => other,
            },
            Step::Release {
                holder,
                key,
                start,
                end,
            } => match state {
                Model::Leased {
                    holder: held,
                    key: leased,
                    start: leased_start,
                    end: leased_end,
                    ..
                }
                | Model::Committed {
                    holder: held,
                    key: leased,
                    start: leased_start,
                    end: leased_end,
                    ..
                } if held == holder
                    && leased == key
                    && leased_start == start
                    && leased_end == end =>
                {
                    Model::Unleased
                }
                other => other,
            },
            Step::Renew {
                holder,
                key,
                deadline,
            } => match state {
                Model::Leased {
                    holder: held,
                    key: leased,
                    start,
                    end,
                    ..
                } if held == holder && leased == key => Model::Leased {
                    holder,
                    key,
                    start,
                    end,
                    deadline,
                },
                other => other,
            },
            Step::Reclaim { now } => match state {
                Model::Leased { deadline, .. } if now >= deadline => Model::Unleased,
                other => other,
            },
        }
    }
}

struct LeaseTest;

impl StateMachineTest for LeaseTest {
    type SystemUnderTest = DeliveryLeaseCell;
    type Reference = LeaseRef;

    fn init_test(
        _ref_state: &<Self::Reference as ReferenceStateMachine>::State,
    ) -> Self::SystemUnderTest {
        DeliveryLeaseCell::new(ChannelId::new(fresh_channel()))
    }

    fn apply(
        cell: Self::SystemUnderTest,
        _ref_state: &<Self::Reference as ReferenceStateMachine>::State,
        transition: <Self::Reference as ReferenceStateMachine>::Transition,
    ) -> Self::SystemUnderTest {
        let channel = cell.channel_id();
        match transition {
            Step::Acquire {
                holder,
                key,
                start,
                end,
                deadline,
            } => {
                let _ = cell.try_acquire(
                    key_of(channel, key),
                    holder_of(holder),
                    u64::from(start),
                    u64::from(end),
                    deadline,
                );
            }
            Step::Commit {
                holder,
                key,
                start,
                end,
                outcome,
            } => {
                let _ = cell.commit(
                    holder_of(holder),
                    key_of(channel, key),
                    u64::from(start),
                    u64::from(end),
                    outcome_of(outcome),
                );
            }
            Step::Release {
                holder,
                key,
                start,
                end,
            } => {
                let _ = cell.release(
                    holder_of(holder),
                    key_of(channel, key),
                    u64::from(start),
                    u64::from(end),
                );
            }
            Step::Renew {
                holder,
                key,
                deadline,
            } => {
                let _ = cell.renew(holder_of(holder), key_of(channel, key), deadline);
            }
            Step::Reclaim { now } => {
                let _ = cell.reclaim_if_expired(now);
            }
        }
        cell
    }

    fn check_invariants(
        cell: &Self::SystemUnderTest,
        model: &<Self::Reference as ReferenceStateMachine>::State,
    ) {
        let channel = cell.channel_id();
        match (*model, cell.read()) {
            (Model::Unleased, LeaseSnapshot::Unleased) => {}
            (
                Model::Leased {
                    holder,
                    key,
                    start,
                    end,
                    deadline,
                },
                LeaseSnapshot::Leased {
                    holder: seen_holder,
                    key: seen_key,
                    deadline_ms,
                    start: seen_start,
                    end: seen_end,
                },
            ) => {
                assert_eq!(seen_holder, holder_of(holder));
                assert_eq!(seen_key, key_of(channel, key));
                assert_eq!(deadline_ms, deadline);
                assert_eq!((seen_start, seen_end), (u64::from(start), u64::from(end)));
            }
            (
                Model::Committed {
                    holder,
                    key,
                    start,
                    end,
                    outcome,
                },
                LeaseSnapshot::Committed {
                    holder: seen_holder,
                    key: seen_key,
                    start: seen_start,
                    end: seen_end,
                    outcome: seen_outcome,
                },
            ) => {
                assert_eq!(seen_holder, holder_of(holder));
                assert_eq!(seen_key, key_of(channel, key));
                assert_eq!((seen_start, seen_end), (u64::from(start), u64::from(end)));
                assert_eq!(seen_outcome, outcome_of(outcome));
            }
            (model, snapshot) => panic!("lease diverged: model={model:?} cell={snapshot:?}"),
        }
    }
}

proptest_state_machine::prop_state_machine! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 48,
        ..proptest::test_runner::Config::default()
    })]
    fn delivery_lease_matches_exclusive_model(sequential 1..24 => LeaseTest);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]
    fn acquire_rejects_empty_or_inverted_range(start in 0u64..30, end in 0u64..30) {
        prop_assume!(start >= end);
        let cell = DeliveryLeaseCell::new(ChannelId::new(fresh_channel()));
        let key = DeliveryLeaseKey::new(cell.channel_id(), 1, 9, None, None);
        prop_assert!(
            !cell.try_acquire(key, LeaseHolder::Sink, start, end, 10),
            "empty or inverted [start, end) must not take the single delivery lease"
        );
        prop_assert!(matches!(cell.read(), LeaseSnapshot::Unleased));
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 40, .. ProptestConfig::default() })]
    fn distinct_id0_turns_do_not_share_a_lease(
        left in "[a-z]{1,8}",
        right in "[a-z]{1,8}",
        offset in 0u64..20,
    ) {
        prop_assume!(left != right);
        let channel = ChannelId::new(fresh_channel());
        let first = DeliveryLeaseKey::new(channel, 3, 0, Some(&left), Some(offset));
        let second = DeliveryLeaseKey::new(channel, 3, 0, Some(&right), None);
        prop_assert_ne!(
            &first,
            &second,
            "id-0 turns that differ in started_at must not collapse to one lease identity"
        );
        let cell = DeliveryLeaseCell::new(channel);
        prop_assert!(cell.try_acquire(first.clone(), LeaseHolder::Sink, 0, 4, 10));
        prop_assert!(
            !cell.commit(LeaseHolder::Sink, second, 0, 4, LeaseOutcome::Delivered),
            "a different id-0 turn must not commit the live lease"
        );
    }
}

pub(super) fn run() -> Vec<String> {
    [
        (
            "delivery_lease_matches_exclusive_model",
            delivery_lease_matches_exclusive_model as fn(),
        ),
        (
            "acquire_rejects_empty_or_inverted_range",
            acquire_rejects_empty_or_inverted_range,
        ),
        (
            "distinct_id0_turns_do_not_share_a_lease",
            distinct_id0_turns_do_not_share_a_lease,
        ),
        (
            "distinct_partial_id0_identities_do_not_alias",
            distinct_partial_id0_identities_do_not_alias,
        ),
        (
            "contended_acquire_has_one_winner_and_stays_coherent",
            contended_acquire_has_one_winner_and_stays_coherent,
        ),
        (
            "reclaim_races_do_not_let_a_stale_holder_commit",
            reclaim_races_do_not_let_a_stale_holder_commit,
        ),
    ]
    .into_iter()
    .filter_map(|(name, test)| super::exercise(name, test))
    .collect()
}

fn distinct_partial_id0_identities_do_not_alias() {
    let channel = ChannelId::new(fresh_channel());
    let first = DeliveryLeaseKey::new(channel, 4, 0, Some("turn-a"), None);
    let second = DeliveryLeaseKey::new(channel, 4, 0, Some("turn-b"), None);
    assert_ne!(
        first, second,
        "id-0 turns with different started_at and no offset collapsed to one lease key"
    );
}

fn contended_acquire_has_one_winner_and_stays_coherent() {
    for round in 0..48 {
        let cell = Arc::new(DeliveryLeaseCell::new(ChannelId::new(fresh_channel())));
        let key = DeliveryLeaseKey::new(cell.channel_id(), 1, 40 + round, None, None);
        thread::scope(|scope| {
            for worker in 0..8u64 {
                let cell = Arc::clone(&cell);
                let key = key.clone();
                scope.spawn(move || {
                    let holder = LeaseHolder::Watcher { instance_id: worker };
                    if cell.try_acquire(key.clone(), holder, 0, 8, 1_000) {
                        let _ = cell.renew(holder, key.clone(), 2_000);
                        if worker % 2 == 0 {
                            assert!(cell.commit(holder, key.clone(), 0, 8, LeaseOutcome::Delivered));
                            assert!(cell.release(holder, key, 0, 8));
                        } else {
                            assert!(cell.release(holder, key, 0, 8));
                        }
                    } else {
                        assert!(!cell.commit(holder, key.clone(), 0, 8, LeaseOutcome::Delivered));
                        assert!(!cell.release(holder, key, 0, 8));
                    }
                });
            }
        });
        match cell.read() {
            LeaseSnapshot::Unleased => {
                let again = DeliveryLeaseKey::new(cell.channel_id(), 1, 90, None, None);
                assert!(
                    cell.try_acquire(again.clone(), LeaseHolder::Sink, 1, 2, 5),
                    "round {round}: unleased cell must accept a new acquire"
                );
                assert!(cell.release(LeaseHolder::Sink, again, 1, 2));
            }
            LeaseSnapshot::Leased {
                holder,
                key,
                start,
                end,
                ..
            } => {
                assert!(cell.commit(holder, key.clone(), start, end, LeaseOutcome::Unknown));
                assert!(!cell.commit(holder, key.clone(), start, end, LeaseOutcome::Delivered));
                assert!(cell.release(holder, key, start, end));
            }
            LeaseSnapshot::Committed {
                holder,
                key,
                start,
                end,
                ..
            } => {
                assert!(cell.release(holder, key, start, end));
            }
        }
    }
}

fn reclaim_races_do_not_let_a_stale_holder_commit() {
    for _ in 0..32 {
        let cell = Arc::new(DeliveryLeaseCell::new(ChannelId::new(fresh_channel())));
        let key = DeliveryLeaseKey::new(cell.channel_id(), 2, 11, None, None);
        assert!(cell.try_acquire(key.clone(), LeaseHolder::Sink, 0, 5, 10));
        let reclaim = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || cell.reclaim_if_expired(10))
        };
        let renew = {
            let cell = Arc::clone(&cell);
            let key = key.clone();
            thread::spawn(move || cell.renew(LeaseHolder::Sink, key, 10))
        };
        let _ = reclaim.join().expect("reclaim thread");
        let _ = renew.join().expect("renew thread");
        let stale = cell.commit(LeaseHolder::Watcher { instance_id: 9 }, key.clone(), 0, 5, LeaseOutcome::Delivered);
        assert!(!stale, "a holder who never acquired must not commit");
        if matches!(cell.read(), LeaseSnapshot::Leased { .. }) {
            assert!(cell.commit(LeaseHolder::Sink, key.clone(), 0, 5, LeaseOutcome::NotDelivered));
        }
        let _ = cell.release(LeaseHolder::Sink, key, 0, 5);
    }
}
