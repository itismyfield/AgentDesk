use super::*;
use crate::services::claude_tui::host_input::guarded_submission::executor_tests::{
    install_shim, renderer, screen,
};
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;

#[test]
fn cold_claude_submit_returns_before_missing_source_and_keeps_payload_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let name = "cold-submit-source-delayed";
    let prompt = "reply + attachments\nvoice _ literal";
    renderer(&f, name, prompt, &screen(""), &screen(prompt), &screen(""));
    let transcript = f.root.path().join("not-created.jsonl");
    let submitted =
        submit(transcript.to_str().unwrap(), 0, None, name, prompt).unwrap_or_else(|error| {
            panic!("cold physical submit must finish without Source: {error:?}")
        });
    assert_eq!(submitted.start_offset, 0);
    assert!(
        !transcript.exists(),
        "cold submit must not wait for a transcript"
    );
    let received = std::fs::read_to_string(f.root.path().join(name).join("frame")).unwrap();
    assert_eq!(received, prompt, "cold Claude preserves prepared bytes");
    let payloads = std::fs::read(f.root.path().join(name).join("payloads")).unwrap();
    let entered = std::fs::read(f.root.path().join(name).join("enters")).unwrap();
    assert_eq!(payloads.len(), 1, "cold Claude has one content effect");
    assert_eq!(entered.len(), 1, "cold Claude enters at most once");
}
