#![cfg(unix)]
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

#[test]
fn fresh_submit_held_and_guarded_never_retry_or_write_payload() {
    use crate::services::claude_tui::host_input::guarded_submission::test_support;
    use crate::services::claude_tui::{
        composer_lock::{DraftGuard, guard_draft},
        host_input::{LegacyTmuxGate, SpyGuard, SpyState},
        input,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let name = "fresh-branch-hold";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
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
        ..Default::default()
    });
    let mut attempts = 0;
    let result = submit_using(
        "/not-created",
        0,
        || {
            attempts += 1;
            input::send_prompt_with_readiness(
                name,
                "own",
                input::PromptReadinessKind::FreshTurn,
                None,
            )
        },
        |_| panic!("Held must not back off"),
        name,
    );
    assert!(
        matches!(result, Err(SubmissionError::Held(_))),
        "fresh Held must remain typed"
    );
    assert_eq!(attempts, 1, "Held must not retry");
    assert!(
        !spy.calls()
            .iter()
            .any(|s| s.starts_with("literal:") || s.starts_with("paste:")),
        "Held has zero payload"
    );
    drop(spy);
    let result =
        test_support::with_request("different-target", std::rc::Rc::new(LegacyTmuxGate), || {
            submit("/not-created", 0, None, name, "own")
        });
    assert!(
        matches!(result.0, Err(SubmissionError::Guarded(_))),
        "fresh Guarded must remain typed"
    );
    assert!(
        !f.root.path().join(name).join("payloads").exists(),
        "Guarded has zero payload"
    );
}

#[test]
fn fresh_readiness_exhaustion_keeps_error_and_exact_attempt_budget() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let detail = "timeout waiting for claude tui fresh prompt input readiness after 120s";
    let mut attempts = 0;
    let mut sleeps = 0;
    let result = submit_using(
        "/not-created",
        0,
        || {
            attempts += 1;
            Err(SubmissionError::Legacy(detail.into()))
        },
        |_| {
            sleeps += 1;
        },
        "fresh-timeout",
    );
    let Err(SubmissionError::Legacy(error)) = result else {
        panic!("readiness exhaustion must stay Legacy");
    };
    assert_eq!(
        attempts, CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS,
        "readiness uses MAX_READY_ATTEMPTS"
    );
    assert_eq!(sleeps, attempts - 1);
    assert_eq!(
        error,
        format!("{detail}; fresh prompt readiness attempts exhausted ({attempts} attempts)"),
        "exhaustion must preserve readiness context"
    );
    assert_eq!(
        exhausted(None),
        format!("claude tui fresh prompt readiness attempts exhausted ({attempts} attempts)"),
        "fallback exhaustion message must remain explicit"
    );
}
