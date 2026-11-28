//! Compact-trigger tests that build the discord-layer managed turn identity, moved out of KRX.

use crate::services::claude_compact_trigger::tests::{armed_state, pane, panes_is_empty};
#[cfg(unix)]
use crate::services::claude_compact_trigger::tests::{observe_and_decide, threshold_for};
use crate::services::claude_compact_trigger::*;
#[cfg(unix)]
use crate::services::claude_compact_trigger::{
    pane_still_disarmed_for_send_for_test as pane_still_disarmed_for_send,
    submit_under_composer_lock_for_test as submit_under_composer_lock,
};
#[cfg(unix)]
use crate::services::claude_tui::readiness::compact::Outcome;
use crate::services::discord::ManagedCompactTurnIdentity;
use crate::services::provider::ProviderKind;

/// How long a wait for something that must happen polls before failing: a hang detector, not a
/// bound of the contract, so a slow runner only makes a broken build fail later.
#[cfg(unix)]
const LIVENESS: std::time::Duration = std::time::Duration::from_secs(10);

#[test]
fn active_usage_missing_model_or_provenance_does_not_create_pane_state() {
    let _guard = state_test_guard();
    assert!(!observe_active_usage(
        ManagedCompactTurnIdentity::test_fixture(42, "tmux-4631"),
        &ProviderKind::Claude,
        None,
        (Some(560_000), Some(0), Some(0)),
        50,
        300_000,
    ));
    assert!(panes_is_empty());
    assert!(!observe_active_usage(
        ManagedCompactTurnIdentity::test_fixture(42, "tmux-4631"),
        &ProviderKind::Claude,
        Some("routed-sonnet[1m]"),
        (Some(560_000), Some(0), Some(0)),
        50,
        300_000,
    ));
    assert!(panes_is_empty());
}

#[test]
fn active_usage_rejects_partial_usage_before_creating_pane_state() {
    let _guard = state_test_guard();
    assert!(!observe_active_usage(
        ManagedCompactTurnIdentity::test_fixture(42, "tmux-4631"),
        &ProviderKind::Claude,
        Some("routed-sonnet[1m]"),
        (Some(560_000), None, Some(0)),
        50,
        300_000,
    ));
    assert!(panes_is_empty());
}

#[test]
fn active_usage_non_claude_provider_does_not_create_pane_state() {
    let _guard = state_test_guard();
    assert!(!observe_active_usage(
        ManagedCompactTurnIdentity::test_fixture(42, "tmux-4631"),
        &ProviderKind::Codex,
        Some("routed-sonnet[1m]"),
        (Some(560_000), Some(0), Some(0)),
        50,
        300_000,
    ));
    assert!(panes_is_empty());
}

/// The worker is parked behind the physical pane's composer lock. While it is
/// parked, the live turn authority changes from the captured Managed identity
/// to ExternalInput or ExternalAdopted. The post-lock authority recheck must
/// reject the stale worker before the submit closure can mutate the composer.
#[cfg(unix)]
#[test]
fn queued_worker_revalidates_live_turn_authority_after_composer_barrier() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[derive(Debug)]
    enum AuthorityTransition {
        ExternalInput,
        ExternalAdopted,
    }

    for external_source in [
        AuthorityTransition::ExternalInput,
        AuthorityTransition::ExternalAdopted,
    ] {
        let _guard = state_test_guard();
        let pane = pane();
        let threshold = threshold_for(1_000_000);
        let generation = observe_and_decide(&pane, 500_000, threshold).expect("crossing injects");
        let expected_turn =
            ManagedCompactTurnIdentity::test_fixture(42, pane.tmux_session_name_for_test());
        let authority_is_managed = Arc::new(AtomicBool::new(true));
        let sends = Arc::new(AtomicUsize::new(0));
        let (queued_tx, queued_rx) = mpsc::channel();
        let (outcome_tx, outcome_rx) = mpsc::channel();
        let worker_pane = pane.clone();
        let worker_authority = Arc::clone(&authority_is_managed);
        let worker_sends = Arc::clone(&sends);

        let worker = crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
            pane.tmux_session_name_for_test(),
            || {
                let worker = std::thread::spawn(move || {
                    queued_tx.send(()).expect("signal queued compact worker");
                    let outcome = submit_under_composer_lock(
                        &worker_pane,
                        generation,
                        threshold,
                        &expected_turn,
                        false,
                        |_| Ok(worker_authority.load(Ordering::SeqCst)),
                        |_permit| {
                            worker_sends.fetch_add(1, Ordering::SeqCst);
                            Outcome::Submitted
                        },
                    );
                    outcome_tx.send(outcome).expect("return compact outcome");
                });
                queued_rx
                    .recv_timeout(LIVENESS)
                    .expect("worker must queue behind the held composer lock");
                assert!(outcome_rx.recv_timeout(Duration::from_millis(25)).is_err());
                authority_is_managed.store(false, Ordering::SeqCst);
                worker
            },
        );
        adk_test_support::test_support::wait::join_within(
            worker,
            adk_test_support::test_support::wait::WAIT_CAP,
        );

        assert_eq!(
            outcome_rx.recv_timeout(LIVENESS).unwrap(),
            None,
            "{external_source:?} must reject the queued worker"
        );
        assert_eq!(
            sends.load(Ordering::SeqCst),
            0,
            "{external_source:?} must cause zero composer mutations"
        );

        let fresh_generation = observe_and_decide(&pane, 500_000, threshold)
            .expect("a later Managed observation must start a fresh high-occupancy cycle");
        assert_ne!(generation, fresh_generation);
        assert!(
            observe_and_decide(&pane, 500_000, threshold).is_none(),
            "the fresh Managed cycle must schedule exactly once"
        );
        assert!(pane_still_disarmed_for_send(
            &pane,
            fresh_generation,
            threshold
        ));
        clear_for_tmux_for_test(pane.tmux_session_name_for_test());
    }
}

/// Degrade-safe no-inject paths never touch the armed flag and never spawn a
/// worker (so these are safe to call without a Tokio runtime): a non-Claude
/// provider, an unresolvable window, and a zero/disabled percent.
#[test]
fn maybe_inject_degrades_safely_without_touching_the_flag() {
    let _guard = state_test_guard();
    let pane = pane();
    maybe_inject_compact(
        ManagedCompactTurnIdentity::test_fixture(
            pane.channel_id_for_test(),
            pane.tmux_session_name_for_test(),
        ),
        &ProviderKind::Codex,
        600_000,
        Some(1_000_000),
        50,
        300_000,
    );
    assert!(armed_state(&pane).is_none());
    maybe_inject_compact(
        ManagedCompactTurnIdentity::test_fixture(
            pane.channel_id_for_test(),
            pane.tmux_session_name_for_test(),
        ),
        &ProviderKind::Claude,
        600_000,
        None,
        50,
        300_000,
    );
    assert!(armed_state(&pane).is_none());
    maybe_inject_compact(
        ManagedCompactTurnIdentity::test_fixture(
            pane.channel_id_for_test(),
            pane.tmux_session_name_for_test(),
        ),
        &ProviderKind::Claude,
        600_000,
        Some(1_000_000),
        0,
        300_000,
    );
    assert!(armed_state(&pane).is_none());
}
