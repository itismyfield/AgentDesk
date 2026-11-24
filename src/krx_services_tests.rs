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

/// A hook relay worker handoff is served before argument parsing: each worker's own error returns.
/// Each handoff runs in a child of this test, so the env change never reaches the test process.
#[test]
fn run_from_args_serves_a_hook_relay_worker_handoff_before_parsing_arguments() {
    use crate::services::claude_tui::hook_relay::{
        failure_marker_worker_env_for_test, non_wait_relay_worker_env_for_test,
    };
    const CELL: &str = "ADK_KRX_WORKER_HANDOFF_PROBE";
    const TEST: &str = "krx_services_tests::run_from_args_serves_a_hook_relay_worker_handoff_before_parsing_arguments";
    const WORKERS: [(&str, &str); 2] = [
        (
            failure_marker_worker_env_for_test(),
            "parse hook relay failure marker handoff",
        ),
        (
            non_wait_relay_worker_env_for_test(),
            "parse non-wait hook relay handoff",
        ),
    ];
    if let Some(cell) = std::env::var_os(CELL) {
        let (worker, parse_error) = WORKERS
            .iter()
            .find(|(worker, _)| cell == *worker)
            .expect("the cell names a worker env");
        let error = crate::run_from_args().expect_err("an unreadable handoff fails in the worker");
        let error = error.to_string();
        assert!(error.starts_with(parse_error), "{worker}: {error}");
        return;
    }
    // A regression that reaches the server path must not find the real runtime root or config.
    let root = tempfile::tempdir().unwrap();
    for (worker, _) in WORKERS {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", TEST, "--nocapture"])
            .env(CELL, worker)
            .env("AGENTDESK_ROOT_DIR", root.path())
            .env("AGENTDESK_CONFIG", root.path().join("agentdesk.yaml"));
        // Only this worker's env is set, so the error names the branch `run_from_args` took.
        for (other, _) in WORKERS {
            child.env_remove(other);
        }
        let output = child.env(worker, "not a handoff").output().unwrap();
        assert!(output.status.success(), "{worker}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "{worker}: {output:?}"
        );
    }
}
