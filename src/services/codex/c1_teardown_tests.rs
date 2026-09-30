#![cfg(unix)]

use super::*;
use crate::services::provider_teardown::tests::test_support::{
    FakeTmux, cleared, refusals, take_exit_reason,
};

fn called(calls: &[String], command: &str) -> bool {
    calls.iter().any(|call| call.starts_with(command))
}

// An existing wrapper session is killed, its files swept and a new one launched only under
// its own clearance; a refusal ends the turn with no audit, kill, sweep or relaunch.
#[test]
fn an_existing_session_is_recreated_only_under_its_clearance() {
    const NAME: &str = "adk-w2a-codex-existing";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = FakeTmux::install(NAME);
    let leftover = crate::services::tmux_common::session_temp_path(NAME, "prompt");
    let cases = std::iter::once(("cleared", Some(cleared(NAME)), true));
    let cases = cases.chain(
        refusals(NAME)
            .into_iter()
            .map(|(label, c)| (label, c, false)),
    );
    for (label, clearance, recreated) in cases {
        crate::services::tmux_common::cleanup_session_temp_files(NAME);
        std::fs::create_dir_all(std::path::Path::new(&leftover).parent().unwrap()).unwrap();
        std::fs::write(&leftover, "stale").unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let result = execute_streaming_local_tmux(
            "hello",
            None,
            None,
            None,
            None,
            "/tmp",
            tx,
            None,
            NAME,
            clearance.as_ref(),
            None,
            None,
            None,
            None,
            true,
        );
        let error = result.expect_err(label);
        assert_eq!(
            error.contains("host guard kept"),
            !recreated,
            "{label}: {error}"
        );
        let calls = tmux.take_calls();
        assert_eq!(
            called(&calls, "capture-pane"),
            recreated,
            "{label}: {calls:?}"
        );
        assert_eq!(called(&calls, "kill-session"), recreated, "{label}");
        assert!(
            !called(&calls, "new-session"),
            "{label}: the CLI never resolves"
        );
        assert_eq!(called(&calls, "reason-before-kill"), recreated, "{label}");
        assert!(recreated || !take_exit_reason(NAME), "{label}");
        let kept = std::fs::read_to_string(&leftover).is_ok_and(|text| text == "stale");
        assert_eq!(kept, !recreated, "{label}: the stale prompt is swept");
    }
}
