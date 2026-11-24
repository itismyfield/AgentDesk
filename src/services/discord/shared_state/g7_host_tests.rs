use crate::services::cluster::intake_worker::IntakeHost;
use crate::services::discord::make_shared_data_for_tests;
use crate::services::discord::session_transition::SessionTransitionHost;
use std::sync::atomic::Ordering;

#[test]
fn the_host_views_are_this_runtimes_own_state() {
    let shared = make_shared_data_for_tests();
    assert!(std::ptr::eq(
        shared.transition_locks(),
        &shared.session_transition_locks
    ));
    assert_eq!(shared.transition_token_hash(), shared.token_hash);
    assert!(std::ptr::eq(
        shared.intake_lifecycle(),
        &shared.restart.intake_worker_lifecycle
    ));
}

#[test]
fn the_shutdown_probe_reads_the_live_flag_of_its_own_runtime() {
    let shared = make_shared_data_for_tests();
    let other = make_shared_data_for_tests();
    let (probe, other_probe) = (shared.shutdown_probe(), other.shutdown_probe());
    assert!(!probe() && !other_probe());
    shared.restart.shutting_down.store(true, Ordering::Release);
    assert!(probe());
    assert!(!other_probe());
}
