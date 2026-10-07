//! A Herdr-configured channel never reaches a Codex tmux path: both tmux entries refuse it before
//! the turn lock, warm follow-up, any tmux read or the cleanup of its session files.

use super::*;

// T1-3: the direct TUI entry and the wrapper entry each refuse with no tmux call, and the session
// files a cleanup would remove are still there.
#[test]
fn both_codex_tmux_entries_refuse_a_herdr_channel_before_any_tmux_call_or_cleanup() {
    use crate::services::herdr_launch::HERDR_NOT_ADMITTED;
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::tests};
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = tests::fixture_after_shared_test_env_lock();
    let _tmux = tests::fake_tmux(root.path());
    let _hosts = crate::config::session_hosts::force_for_test(None, &[(52, "mac-mini")]);
    let tmux = "AgentDesk-codex-herdr-guard";
    let leftovers = ["jsonl", "sh", "input"]
        .map(|ext| crate::services::tmux_common::session_temp_path(tmux, ext));
    for path in &leftovers {
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, "kept").unwrap();
    }
    for wrapper in [false, true] {
        let (sender, _receiver) = std::sync::mpsc::channel();
        let refused = match wrapper {
            false => execute_streaming_local_tui_tmux(
                "q",
                None,
                None,
                None,
                None,
                "/tmp",
                sender,
                None,
                tmux,
                None,
                Some(52),
                None,
                None,
                false,
                None,
                false,
            ),
            true => execute_streaming_local_tmux(
                "q",
                None,
                None,
                None,
                None,
                "/tmp",
                sender,
                None,
                tmux,
                None,
                Some(52),
                None,
                None,
                None,
                false,
            ),
        };
        assert_eq!(
            refused,
            Err(HERDR_NOT_ADMITTED.to_string()),
            "wrapper={wrapper}"
        );
        for path in &leftovers {
            assert!(std::path::Path::new(path).exists(), "{path} removed");
        }
        assert!(
            !root.path().join("tmux.calls").exists(),
            "wrapper={wrapper}"
        );
    }
}
