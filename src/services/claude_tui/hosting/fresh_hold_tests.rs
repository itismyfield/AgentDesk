use super::*;
use crate::services::claude_tui::composer_lock::{DraftGuard, guard_draft};
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
use crate::services::claude_tui::input::submission::tests::{frame_renderer as renderer, screen};
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;

#[test]
fn a2_fresh_readiness_hold_never_kills_the_session_or_owner_marker() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let name = "a2-fresh-held";
    let transcript = f.root.path().join("a2-fresh.jsonl");
    let owner = f.root.path().join("a2-owner");
    std::fs::write(&transcript, "").unwrap();
    std::fs::write(&owner, "owner").unwrap();
    let previous = f.root.path().join("previous-native.jsonl");
    std::fs::write(&previous, "old transcript\n").unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        name,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: previous.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let calls = f.root.path().join("a2-calls");
    let shim = f.root.path().join("tmux");
    let script = std::fs::read_to_string(&shim).unwrap();
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{}",
            calls.display(),
            script.strip_prefix("#!/bin/sh\n").unwrap()
        ),
    )
    .unwrap();
    for _ in 0..2 {
        if crate::services::platform::tmux::session_presence(name)
            != crate::services::platform::tmux::SessionPresence::Present
        {
            let prior = f.root.path().join(name);
            if prior.exists() {
                std::fs::rename(&prior, f.root.path().join(format!("{name}-retired"))).unwrap();
            }
            renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
        }
        guard_draft(name, DraftGuard::RecoveryRequired);
        let spy = SpyGuard::install(SpyState {
            captures: [
                Some(screen("")),
                Some(screen("")),
                Some(screen("")),
                Some(screen("")),
                Some("unknown overlay".into()),
            ]
            .into_iter()
            .collect(),
            draft_capture_at: Some((1, Some("unknown overlay".into()))),
            ..SpyState::default()
        });
        let (tx, _) = std::sync::mpsc::channel();
        let result = run_claude_tui_fresh_turn_and_finalize(
            &transcript,
            transcript.to_str().unwrap(),
            tx,
            Some(std::sync::Arc::new(CancelToken::new())),
            name,
            Some(&TeardownClearance::Unkeyed),
            "held-sid",
            None,
            "own",
            owner.to_str().unwrap(),
        );
        assert!(result.is_err());
        assert!(
            !spy.calls()
                .iter()
                .any(|c| c.starts_with("literal:") || c.starts_with("paste:"))
        );
    }
    let calls = std::fs::read_to_string(&calls).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("kill-session"))
            .count(),
        0,
        "preparation holds must not kill: {calls}"
    );
    assert!(owner.exists(), "the held session retains its owner marker");
    assert_eq!(
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(name)
            .unwrap()
            .output_path,
        previous.display().to_string(),
        "preparation hold retains the old binding without killing or resetting it"
    );
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(name);
    assert_eq!(
        crate::services::platform::tmux::session_presence(name),
        crate::services::platform::tmux::SessionPresence::Present
    );
}
