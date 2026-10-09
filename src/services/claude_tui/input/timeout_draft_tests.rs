//! A follow-up readiness timeout on a pane whose composer holds text AgentDesk did not type.

use super::*;
use crate::services::claude_tui::composer_lock::draft_guarded;
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};

/// An idle pane whose two-row draft says "running", which the plain reader calls a draft.
fn two_row_draft() -> String {
    let border = "\u{2500}".repeat(60);
    let footer = "  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)";
    let rows = "\u{276f}\u{a0}Review this task\n  The job keeps running forever";
    format!("\u{23fa} Done.\n\n\n{border}\n{rows}\n{border}\n{footer}\n")
}

/// Runs `send` on a fresh session over `pane` with a zero follow-up timeout; returns its result,
/// the pane writes and the session.
fn timed_out(
    pane: &str,
    send: impl FnOnce(&str, &CancelToken) -> Result<(), String>,
) -> (Result<(), String>, Vec<String>, String) {
    let _env = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let session = format!("timeout-draft-{}", uuid::Uuid::new_v4().simple());
    FOLLOWUP_TIMEOUT_FOR_TESTS.with(|timeout| timeout.set(Some(Duration::ZERO)));
    let spy = SpyGuard::install(SpyState {
        captures: std::iter::repeat_n(Some(pane.to_string()), 40).collect(),
        ..SpyState::default()
    });
    let result = send(&session, &CancelToken::new());
    FOLLOWUP_TIMEOUT_FOR_TESTS.with(|timeout| timeout.set(None));
    let write = |call: &String| {
        ["keys:", "literal:", "load:", "paste:", "retire:"]
            .iter()
            .any(|kind| call.starts_with(kind))
    };
    let writes = spy.calls().into_iter().filter(write).collect();
    (result, writes, session)
}

/// A plain follow-up and a selector follow-up that time out behind a person's draft send no key,
/// leave the draft, protect the pane and return the hold the turn bridge requeues.
#[test]
fn a_follow_up_timing_out_on_a_person_draft_keeps_every_key() {
    let pane = two_row_draft();
    assert!(prompt_readiness_snapshot_from_capture(Some(&pane), true).prompt_draft_detected);
    let nav = SelectorNavigation {
        slash_command: "/effort",
        total_items: 5,
        target_index: 2,
    };
    let plain =
        |session: &str, token: &CancelToken| send_followup_prompt(session, "/cost", Some(token));
    let selector =
        |session: &str, token: &CancelToken| send_selector_followup(session, nav, Some(token));
    type Send<'a> = &'a dyn Fn(&str, &CancelToken) -> Result<(), String>;
    let sends: [(&str, Send); 2] = [("follow-up", &plain), ("selector", &selector)];
    for (name, send) in sends {
        let (ended, writes, session) = timed_out(&pane, send);
        assert_eq!(writes, Vec::<String>::new(), "{name}");
        let error = ended.expect_err(name);
        assert!(is_prompt_ready_timeout_error(&error), "{name}: {error}");
        assert!(
            error.contains("reason=draft_recovery_hold"),
            "{name}: {error}"
        );
        assert!(draft_guarded(&session), "{name}");
    }
}
