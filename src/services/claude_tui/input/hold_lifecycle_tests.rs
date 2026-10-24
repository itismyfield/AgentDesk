use super::*;
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};

fn empty() -> String {
    let border = "─".repeat(60);
    format!("⏺ Done.\n{border}\n❯ \n{border}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n")
}

#[test]
fn a2_legacy_confirmation_never_repeats_enter_into_a_remaining_draft() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let session = format!("a2-confirm-{}", uuid::Uuid::new_v4());
    let draft = empty().replace("❯ \n", "❯ 첫째 줄\n  원문 보존\n");
    let spy = SpyGuard::install(SpyState {
        captures: std::iter::repeat_n(Some(empty()), 30).collect(),
        captures_after_send: Some(std::iter::repeat_n(Some(draft), 10).collect()),
        ..SpyState::default()
    });
    let result = send_fresh_prompt(&session, "첫째 줄\n원문 보존", Some(&CancelToken::new()));
    let calls = spy.calls();
    assert_eq!(
        calls.iter().filter(|c| c.as_str() == "keys:Enter").count(),
        1,
        "an ambiguous submit gets no second Enter: {calls:?}"
    );
    assert!(result.is_err());
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("C-u") || c.contains("Escape") || c.contains("BSpace"))
    );
}

#[test]
fn a2_legacy_send_errors_never_erase_uncertain_paste() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for fail_at in [Some(1), Some(2), None] {
        let session = format!("a2-in-send-{}", uuid::Uuid::new_v4());
        let spy = SpyGuard::install(SpyState {
            captures: std::iter::repeat_n(Some(empty()), 30).collect(),
            captures_after_send: Some(std::collections::VecDeque::new()),
            fail_send: fail_at.map(|at| (at, Err("uncertain transport ACK".into()))),
            ..SpyState::default()
        });
        let result = send_fresh_prompt(&session, "첫째 줄\n원문 보존", Some(&CancelToken::new()));
        assert!(result.is_err(), "{fail_at:?}");
        let calls = spy.calls();
        let cleanup: Vec<_> = calls
            .iter()
            .filter(|call| {
                call.starts_with("keys:")
                    && (call.contains("C-u") || call.contains("Escape") || call.contains("BSpace"))
            })
            .collect();
        assert!(
            cleanup.is_empty(),
            "uncertain submission must preserve raw payload: {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("paste:"))
                .count(),
            1
        );
    }
}

#[test]
fn a2_last_cleanup_capture_must_still_prove_the_draft_is_ours() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for capture in [
        None,
        Some("unrecognized overlay".into()),
        Some(empty().replace("❯ \n", "❯ person's draft\n")),
    ] {
        let session = format!("a2-cleanup-owner-{}", uuid::Uuid::new_v4());
        let spy = SpyGuard::install(SpyState {
            captures: [capture].into(),
            ..SpyState::default()
        });
        clear_prompt_draft_before_error(&session);
        assert!(
            !spy.calls().iter().any(|c| c.starts_with("keys:")),
            "unknown and person drafts get no cleanup: {:?}",
            spy.calls()
        );
    }
}

#[test]
fn a2_cancel_before_payload_keeps_the_existing_cancellation_outcome() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let session = format!("a2-cancel-before-{}", uuid::Uuid::new_v4());
    let token = CancelToken::new();
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let spy = SpyGuard::install(SpyState::default());
    let result = guarded_outcome::capture_preparation(|| {
        run_actions_with_submission_confirmation(
            &session,
            &[
                TuiInputAction::PasteBuffer("raw\ninput".into()),
                TuiInputAction::Enter,
            ],
            Some("raw\ninput"),
            Some(&token),
        )
        .map_err(SubmissionError::from)
    });
    assert!(
        matches!(result, Err(SubmissionError::Legacy(_))),
        "cancellation before payload is not an uncertain submission: {result:?}"
    );
    assert!(!crate::services::claude_tui::composer_lock::draft_guarded(
        &session
    ));
    assert!(
        !spy.calls()
            .iter()
            .any(|c| c.starts_with("keys:") || c.starts_with("paste:"))
    );
}
