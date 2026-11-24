//! KRX services tests that call root-crate items, declared here so they leave the KRX crate.
//! Each file stays in place; every `mod` keeps the `cfg(test)` chain of its original declaration.

#[path = "services/cluster/intake_router_hook/o_input/fallback_route_tests.rs"]
mod cluster_fallback_route;
#[path = "services/cluster/intake_worker/o_route_tests.rs"]
mod cluster_intake_worker_o_route;
#[path = "services/cluster/intake_worker/owner_fence_pg_tests.rs"]
mod cluster_intake_worker_owner_fence_pg;
#[path = "services/cluster/intake_worker/replay_hold_tests.rs"]
mod cluster_intake_worker_replay_hold;
#[path = "services/cluster/intake_router_hook/owner_record/event_tests.rs"]
mod cluster_owner_record_event;

/// A hook relay worker handoff is served before argument parsing: the worker's own error returns.
#[test]
fn run_from_args_serves_a_hook_relay_worker_handoff_before_parsing_arguments() {
    use crate::config::TestEnvVarGuard as Guard;
    use crate::services::claude_tui::hook_relay::FAILURE_MARKER_WORKER_ENV;
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    // A regression that reaches the server path must not find the real runtime root.
    let root = tempfile::tempdir().unwrap();
    let _root = Guard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
    let _handoff = Guard::set_value_after_shared_test_env_lock(
        FAILURE_MARKER_WORKER_ENV,
        "not a handoff".as_ref(),
    );
    let error = crate::run_from_args().expect_err("an unreadable handoff fails in the worker");
    let error = error.to_string();
    assert!(
        error.starts_with("parse hook relay failure marker handoff"),
        "{error}"
    );
}
