//! Turn finalizer ledger: duplicate, out-of-order, concurrent, and restart sequences.

use std::sync::Arc;

use proptest::prelude::*;
use serenity::model::id::ChannelId;

use super::{IsolatedRoot, fresh_channel, runtime};
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::turn_finalizer::{
    FinalizeContext, FinalizeOutcome, TerminalEvent, TurnFinalizer, TurnKey,
};
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug)]
enum FinOp {
    Register { msg: u8, watcher: bool },
    Complete { msg: u8 },
    Cancel { msg: u8 },
    GateBusy { msg: u8 },
    IdZero,
}

fn op_strategy() -> impl Strategy<Value = FinOp> {
    prop_oneof![
        (1u8..5, any::<bool>()).prop_map(|(msg, watcher)| FinOp::Register { msg, watcher }),
        (1u8..5u8).prop_map(|msg| FinOp::Complete { msg }),
        (1u8..5u8).prop_map(|msg| FinOp::Cancel { msg }),
        (1u8..5u8).prop_map(|msg| FinOp::GateBusy { msg }),
        Just(FinOp::IdZero),
    ]
}

fn key(channel: u64, msg: u64) -> TurnKey {
    TurnKey::new(ChannelId::new(channel), msg, 0)
}

fn is_finalized(outcome: &FinalizeOutcome) -> bool {
    matches!(outcome, FinalizeOutcome::Finalized { .. })
}

fn show_outcome(outcome: &FinalizeOutcome) -> &'static str {
    match outcome {
        FinalizeOutcome::Finalized { .. } => "Finalized",
        FinalizeOutcome::AlreadyFinalized => "AlreadyFinalized",
        FinalizeOutcome::Deferred => "Deferred",
    }
}

async fn drive(channel: u64, ops: &[FinOp]) {
    let shared = super::super::make_shared_data_for_tests();
    let fin = TurnFinalizer::spawn();
    let mut owned_finalize = [0u8; 5];
    let mut registered = [false; 5];
    let mut watcher = [false; 5];
    for op in ops {
        match *op {
            FinOp::Register { msg, watcher: is_watcher } => {
                registered[msg as usize] = true;
                let owner = if is_watcher {
                    RelayOwnerKind::Watcher
                } else {
                    RelayOwnerKind::None
                };
                watcher[msg as usize] = is_watcher;
                fin.register_start(
                    key(channel, u64::from(msg)),
                    ProviderKind::Claude,
                    owner,
                    &shared,
                );
                tokio::task::yield_now().await;
            }
            FinOp::Complete { msg } | FinOp::Cancel { msg } => {
                let event = if matches!(op, FinOp::Cancel { .. }) {
                    TerminalEvent::Cancel
                } else {
                    TerminalEvent::Complete
                };
                let outcome = fin
                    .submit_terminal(
                        key(channel, u64::from(msg)),
                        ProviderKind::Claude,
                        event,
                        FinalizeContext::watcher(),
                        Arc::clone(&shared),
                    )
                    .await;
                let slot = &mut owned_finalize[msg as usize];
                if is_finalized(&outcome) {
                    assert!(
                        *slot == 0,
                        "message {msg} finalized twice; ops={ops:?}"
                    );
                    *slot = 1;
                }
                if *slot == 1 && matches!(op, FinOp::Complete { .. } | FinOp::Cancel { .. }) {
                    let again = fin
                        .submit_terminal(
                            key(channel, u64::from(msg)),
                            ProviderKind::Claude,
                            TerminalEvent::Complete,
                            FinalizeContext::watcher(),
                            Arc::clone(&shared),
                        )
                        .await;
                    assert!(
                        matches!(again, FinalizeOutcome::AlreadyFinalized),
                        "duplicate complete after finalize must not own the turn; got {}",
                        show_outcome(&again)
                    );
                }
            }
            FinOp::GateBusy { msg } => {
                let outcome = fin
                    .submit_terminal(
                        key(channel, u64::from(msg)),
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        Arc::clone(&shared),
                    )
                    .await;
                if is_finalized(&outcome) {
                    owned_finalize[msg as usize] = 1;
                }
                if watcher[msg as usize] && owned_finalize[msg as usize] == 0 {
                    assert!(
                        matches!(outcome, FinalizeOutcome::Deferred),
                        "busy watcher gate must defer, not drop or double-finalize; got {}",
                        show_outcome(&outcome)
                    );
                    assert!(
                        fin.has_live_watcher_pending(ChannelId::new(channel), 0)
                            .await,
                        "deferred watcher turn stuck as not pending"
                    );
                }
            }
            FinOp::IdZero => {
                let live = (1..5)
                    .filter(|msg| registered[*msg] && owned_finalize[*msg] == 0)
                    .count();
                let outcome = fin
                    .submit_terminal(
                        key(channel, 0),
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        Arc::clone(&shared),
                    )
                    .await;
                if live == 1 && is_finalized(&outcome) {
                    for msg in 1..5 {
                        if registered[msg] && owned_finalize[msg] == 0 {
                            owned_finalize[msg] = 1;
                        }
                    }
                }
                if live >= 2 {
                    assert!(
                        matches!(outcome, FinalizeOutcome::AlreadyFinalized),
                        "ambiguous id-0 terminal must not pick a live turn; got {}",
                        show_outcome(&outcome)
                    );
                    for msg in 1..5 {
                        if owned_finalize[msg] == 0 {
                            let still = fin
                                .submit_terminal(
                                    key(channel, u64::from(msg as u64)),
                                    ProviderKind::Claude,
                                    TerminalEvent::Complete,
                                    FinalizeContext::watcher(),
                                    Arc::clone(&shared),
                                )
                                .await;
                            if is_finalized(&still) {
                                owned_finalize[msg] = 1;
                            }
                        }
                    }
                }
            }
        }
    }
    for msg in 1..5 {
        if owned_finalize[msg] == 1 {
            let late = fin
                .submit_terminal(
                    key(channel, u64::from(msg as u64)),
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    Arc::clone(&shared),
                )
                .await;
            assert!(
                matches!(late, FinalizeOutcome::AlreadyFinalized),
                "late duplicate from another submitter re-owned message {msg}: {}",
                show_outcome(&late)
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]
    fn finalizer_sequences_finalize_each_turn_once(
        ops in prop::collection::vec(op_strategy(), 1..18)
    ) {
        let _root = IsolatedRoot::acquire();
        let channel = fresh_channel();
        runtime().block_on(drive(channel, &ops));
    }
}

pub(super) fn run() -> Vec<String> {
    [
        (
            "finalizer_sequences_finalize_each_turn_once",
            finalizer_sequences_finalize_each_turn_once as fn(),
        ),
        (
            "concurrent_duplicate_terminals_have_one_owner",
            concurrent_duplicate_terminals_have_one_owner,
        ),
        (
            "restart_replays_a_finalized_turn_as_already_finalized",
            restart_replays_a_finalized_turn_as_already_finalized,
        ),
        (
            "stale_generation_terminal_is_not_dropped",
            stale_generation_terminal_is_not_dropped,
        ),
        (
            "episode_terminal_without_nonce_stays_retryable",
            episode_terminal_without_nonce_stays_retryable,
        ),
        (
            "gate_defer_then_complete_is_not_stuck",
            gate_defer_then_complete_is_not_stuck,
        ),
    ]
    .into_iter()
    .filter_map(|(name, test)| super::exercise(name, test))
    .collect()
}

fn concurrent_duplicate_terminals_have_one_owner() {
    let _root = IsolatedRoot::acquire();
    let rt = runtime();
    rt.block_on(async {
        for round in 0..24 {
            let shared = super::super::make_shared_data_for_tests();
            let fin = TurnFinalizer::spawn();
            let channel = fresh_channel();
            let turn = key(channel, 70 + round);
            fin.register_start(turn, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);
            tokio::task::yield_now().await;
            let mut joins = Vec::new();
            for _ in 0..8 {
                let fin = Arc::clone(&fin);
                let shared = Arc::clone(&shared);
                joins.push(tokio::spawn(async move {
                    fin.submit_terminal(
                        turn,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        shared,
                    )
                    .await
                }));
            }
            let mut owners = 0;
            for join in joins {
                if is_finalized(&join.await.expect("terminal task")) {
                    owners += 1;
                }
            }
            assert_eq!(owners, 1, "round {round}: concurrent completes");
            assert!(
                !fin.has_live_watcher_pending(ChannelId::new(channel), 0).await,
                "round {round}: finalized watcher turn still pending"
            );
        }
    });
}

fn restart_replays_a_finalized_turn_as_already_finalized() {
    let _root = IsolatedRoot::acquire();
    runtime().block_on(async {
        let shared = super::super::make_shared_data_for_tests();
        let channel = fresh_channel();
        let turn = key(channel, 81);
        let first = {
            let fin = TurnFinalizer::spawn();
            fin.register_start(turn, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);
            tokio::task::yield_now().await;
            let outcome = fin
                .submit_terminal(
                    turn,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    Arc::clone(&shared),
                )
                .await;
            assert!(is_finalized(&outcome), "first complete: {}", show_outcome(&outcome));
            outcome
        };
        tokio::task::yield_now().await;
        let restarted = TurnFinalizer::spawn();
        let second = restarted
            .submit_terminal(
                turn,
                ProviderKind::Claude,
                TerminalEvent::Complete,
                FinalizeContext::watcher(),
                shared,
            )
            .await;
        assert!(
            matches!(second, FinalizeOutcome::AlreadyFinalized),
            "restart sequence [register, complete, drop actor, complete] re-owned the turn. \
             first={} second={}",
            show_outcome(&first),
            show_outcome(&second)
        );
    });
}

fn stale_generation_terminal_is_not_dropped() {
    let _root = IsolatedRoot::acquire();
    runtime().block_on(async {
        let shared = super::super::make_shared_data_for_tests();
        let channel = fresh_channel();
        let turn = TurnKey::new(ChannelId::new(channel), 15, 1);
        let fin = TurnFinalizer::spawn();
        let outcome = fin
            .submit_terminal(
                turn,
                ProviderKind::Claude,
                TerminalEvent::Complete,
                FinalizeContext::watcher(),
                Arc::clone(&shared),
            )
            .await;
        assert!(
            is_finalized(&outcome),
            "a sole terminal for generation 1 while the boot generation is 0 must still finalize \
             exactly once, not report AlreadyFinalized and leave the turn unrecorded. got {}",
            show_outcome(&outcome)
        );
        let duplicate = fin
            .submit_terminal(
                turn,
                ProviderKind::Claude,
                TerminalEvent::Complete,
                FinalizeContext::watcher(),
                shared,
            )
            .await;
        assert!(
            matches!(duplicate, FinalizeOutcome::AlreadyFinalized),
            "the same stale-generation terminal must not finalize twice: {}",
            show_outcome(&duplicate)
        );
    });
}

fn episode_terminal_without_nonce_stays_retryable() {
    let _root = IsolatedRoot::acquire();
    runtime().block_on(async {
        let shared = super::super::make_shared_data_for_tests();
        let channel = fresh_channel();
        let registered = key(channel, 22).with_episode_nonce(Some("episode-a"));
        let fin = TurnFinalizer::spawn();
        fin.register_start(
            registered,
            ProviderKind::Claude,
            RelayOwnerKind::Watcher,
            &shared,
        );
        tokio::task::yield_now().await;
        let bare = fin
            .submit_terminal(
                key(channel, 22),
                ProviderKind::Claude,
                TerminalEvent::Complete,
                FinalizeContext::watcher(),
                Arc::clone(&shared),
            )
            .await;
        assert!(
            matches!(bare, FinalizeOutcome::Deferred),
            "episode-less terminal must not finalize a bound episode or pretend it did: {}",
            show_outcome(&bare)
        );
        assert!(
            fin.has_live_watcher_pending(ChannelId::new(channel), 0).await,
            "deferred episode turn disappeared"
        );
        let matched = fin
            .submit_terminal(
                registered,
                ProviderKind::Claude,
                TerminalEvent::Complete,
                FinalizeContext::watcher(),
                Arc::clone(&shared),
            )
            .await;
        assert!(
            is_finalized(&matched),
            "matching episode terminal after a nonce-less duplicate stayed stuck: bare={} matched={}",
            show_outcome(&bare),
            show_outcome(&matched)
        );
        assert!(!fin.has_live_watcher_pending(ChannelId::new(channel), 0).await);
    });
}

fn gate_defer_then_complete_is_not_stuck() {
    let _root = IsolatedRoot::acquire();
    runtime().block_on(async {
        let shared = super::super::make_shared_data_for_tests();
        let channel = fresh_channel();
        let turn = key(channel, 33);
        let fin = TurnFinalizer::spawn();
        fin.register_start(turn, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);
        tokio::task::yield_now().await;
        let deferred = fin
            .submit_terminal(
                turn,
                ProviderKind::Claude,
                TerminalEvent::GateTimeout {
                    pane_quiescent: Some(false),
                },
                FinalizeContext::watcher(),
                Arc::clone(&shared),
            )
            .await;
        assert!(matches!(deferred, FinalizeOutcome::Deferred));
        let done = fin
            .submit_terminal(
                turn,
                ProviderKind::Claude,
                TerminalEvent::Complete,
                FinalizeContext::watcher(),
                Arc::clone(&shared),
            )
            .await;
        assert!(is_finalized(&done), "complete after defer: {}", show_outcome(&done));
        let late = fin
            .submit_terminal(
                turn,
                ProviderKind::Claude,
                TerminalEvent::GateTimeout {
                    pane_quiescent: Some(false),
                },
                FinalizeContext::watcher(),
                shared,
            )
            .await;
        assert!(
            matches!(late, FinalizeOutcome::AlreadyFinalized),
            "gate timeout after finalize re-opened the turn: {}",
            show_outcome(&late)
        );
    });
}
