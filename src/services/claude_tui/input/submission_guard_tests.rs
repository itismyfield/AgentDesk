use super::*;
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};

const IDLE: &str = include_str!("../../../../tests/fixtures/tui_input/claude-2.1.293-idle.txt");
const PROMPT: &str = "한글\n줄바꿈";
const OWN: &str = "────────────────────\n❯ 한글\n  줄바꿈\n────────────────────\n";
const FOLDED: &str = "────────────────────\n❯ [Pasted text #7 +1 lines]\n────────────────────\n";
const FOREIGN: &str = "────────────────────\n❯ 사람의 초안\n────────────────────\n";
const FOREIGN_CONTINUATION_OWN: &str =
    "────────────────────\n❯ foreign\n  ❯ 한글\n  줄바꿈\n────────────────────\n";
const EMPTY_FIRST_FOREIGN_CONTINUATION: &str =
    "────────────────────\n❯ \n  foreign continuation\n────────────────────\n";
const EMPTY_FIRST_MALFORMED_CONTINUATION: &str =
    "────────────────────\n❯ \nunindented foreign continuation\n────────────────────\n";
const INDENTED_EMPTY_MARKER: &str = "────────────────────\n  ❯ \n────────────────────\n";
const BUSY: &str = "✳ Architecting…";
const BUSY_USER_DRAFT: &str =
    "✳ Architecting…\n────────────────────\n❯ [User: A (ID:1)] stranded\n────────────────────\n";
const BUSY_TRUNCATED_COMPOSER: &str = "✳ Architecting…\n❯ \n";
const BUSY_MALFORMED_COMPOSER: &str =
    "✳ Architecting…\n❯ \nunindented continuation\n────────────────────\n";
const BUSY_PROMPT_CONTINUATION: &str =
    "✳ Architecting…\n────────────────────\n❯ foreign\n  ❯ \n────────────────────\n";
const MODAL: &str =
    "Do you want to make this edit?\n❯ 1. Yes\n  2. No\nEnter to confirm · Esc to cancel";

#[derive(Clone, Copy, Debug)]
enum Entry {
    Fresh,
    Followup,
    ProvenWarm,
    Steering,
}

const ENTRIES: [Entry; 4] = [
    Entry::Fresh,
    Entry::Followup,
    Entry::ProvenWarm,
    Entry::Steering,
];

fn submit(entry: Entry, session: &str, token: Option<&CancelToken>) -> Result<(), String> {
    // Hosted dispatch supplies a token; its polling path stays on the thread owning this spy.
    let local = CancelToken::new();
    let token = Some(token.unwrap_or(&local));
    match entry {
        Entry::Fresh => send_fresh_prompt(session, PROMPT, token),
        Entry::Followup => send_followup_prompt(session, PROMPT, token),
        Entry::Steering => inject_steering_prompt(session, PROMPT),
        Entry::ProvenWarm => {
            let transcript = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(
                transcript.path(),
                "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
            )
            .unwrap();
            send_followup_prompt_or_idle_transcript(session, PROMPT, token, transcript.path())
        }
    }
}

fn state(after: &[Option<&str>]) -> SpyState {
    SpyState {
        captures: std::iter::repeat_n(Some(IDLE.to_string()), 16).collect(),
        captures_after_send: Some(after.iter().map(|s| s.map(str::to_string)).collect()),
        ..SpyState::default()
    }
}

fn assert_no_cleanup(calls: &[String]) {
    assert!(
        !calls.iter().any(|call| {
            call == "keys:C-u"
                || call == "keys:Escape"
                || call.starts_with("keys:BSpace")
                || call.starts_with("retire:")
        }),
        "{calls:?}"
    );
}

fn assert_late_hold(error: &str) {
    assert!(
        error.starts_with("claude tui input held after mutation:"),
        "{error}"
    );
    // The turn bridge only requeues the pre-submit readiness-timeout error family.
    assert!(!is_prompt_ready_timeout_error(error), "{error}");
    assert!(
        !error.contains("follow-up prompt input readiness"),
        "{error}"
    );
}

#[test]
fn claude_actual_entries_withhold_enter_after_late_foreign_modal_or_blind_capture() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for entry in ENTRIES {
        for after in [
            Some(FOREIGN),
            Some(MODAL),
            Some(FOREIGN_CONTINUATION_OWN),
            None,
        ] {
            let session = format!("claude-guard-late-{}", uuid::Uuid::new_v4());
            let spy = SpyGuard::install(state(&[after]));
            let error = submit(entry, &session, None).unwrap_err();
            assert_late_hold(&error);
            let calls = spy.calls();
            assert_eq!(
                calls.iter().filter(|c| *c == "keys:Enter").count(),
                0,
                "{entry:?}: {calls:?}"
            );
            assert_eq!(
                calls.iter().filter(|c| *c == "paste:delete=true").count(),
                1
            );
            assert_no_cleanup(&calls);
        }
    }
}

#[test]
fn claude_actual_entries_submit_owned_expanded_and_folded_drafts_once() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for entry in ENTRIES {
        for own in [OWN, FOLDED] {
            let session = format!("claude-guard-owned-{}", uuid::Uuid::new_v4());
            let spy = SpyGuard::install(state(&[Some(own), Some(BUSY)]));
            assert_eq!(submit(entry, &session, None), Ok(()), "{entry:?}");
            let calls = spy.calls();
            assert_eq!(
                calls.iter().filter(|c| *c == "keys:Enter").count(),
                1,
                "{entry:?}: {calls:?}"
            );
            let paste = calls.iter().position(|c| c == "paste:delete=true").unwrap();
            let enter = calls.iter().position(|c| c == "keys:Enter").unwrap();
            assert_eq!(&calls[paste + 1..enter], ["capture:draft", "alive"]);
            assert_no_cleanup(&calls);
        }
    }
}

#[test]
fn claude_actual_entries_leave_post_enter_drafts_and_blind_confirmation_untouched() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let colored_foreign =
        "✳ Architecting…\n────────────────────\n\x1b[39m❯\x1b[0m 人の草稿\n────────────────────\n";
    for entry in ENTRIES {
        for confirmation in [
            Some(OWN),
            Some(colored_foreign),
            Some(BUSY_USER_DRAFT),
            Some(BUSY_TRUNCATED_COMPOSER),
            Some(BUSY_MALFORMED_COMPOSER),
            Some(BUSY_PROMPT_CONTINUATION),
            None,
        ] {
            let session = format!("claude-guard-confirm-{}", uuid::Uuid::new_v4());
            let spy = SpyGuard::install(state(&[Some(OWN), confirmation]));
            let error = submit(entry, &session, None).unwrap_err();
            assert_late_hold(&error);
            let calls = spy.calls();
            assert_eq!(
                calls.iter().filter(|c| *c == "keys:Enter").count(),
                1,
                "{entry:?}: {calls:?}"
            );
            let enter = calls.iter().position(|c| c == "keys:Enter").unwrap();
            assert_eq!(calls[enter + 1], "capture:draft", "{calls:?}");
            assert_eq!(
                calls[enter + 1..]
                    .iter()
                    .filter(|c| c.starts_with("capture"))
                    .count(),
                1,
                "{calls:?}"
            );
            assert_no_cleanup(&calls);
        }
    }
}

#[test]
fn claude_actual_entries_recheck_raw_empty_composer_before_payload() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for entry in ENTRIES {
        for changed in [
            Some(FOREIGN),
            Some(MODAL),
            None,
            Some(EMPTY_FIRST_FOREIGN_CONTINUATION),
            Some(EMPTY_FIRST_MALFORMED_CONTINUATION),
            Some(INDENTED_EMPTY_MARKER),
        ] {
            let session = format!("claude-guard-pre-{}", uuid::Uuid::new_v4());
            let mut setup = state(&[Some(BUSY)]);
            let draft_read = match entry {
                Entry::Fresh | Entry::Steering => 1,
                Entry::Followup | Entry::ProvenWarm => 2,
            };
            setup.draft_capture_at = Some((draft_read, changed.map(str::to_string)));
            let spy = SpyGuard::install(setup);
            let result = submit(entry, &session, None);
            let calls = spy.calls();
            assert!(
                !calls.iter().any(|c| c.starts_with("literal:")
                    || c.starts_with("load:")
                    || c.starts_with("paste:")
                    || c.starts_with("keys:")),
                "{entry:?}: {result:?}: {calls:?}"
            );
            let error = result.unwrap_err();
            assert!(
                error.starts_with("claude tui input refused before mutation:"),
                "{entry:?}: {error}"
            );
            assert!(!is_prompt_ready_timeout_error(&error));
            assert_no_cleanup(&calls);
        }
    }
}

#[test]
fn claude_actual_entries_never_retry_or_clear_after_uncertain_enter_ack() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for entry in ENTRIES {
        let session = format!("claude-guard-ack-{}", uuid::Uuid::new_v4());
        let mut setup = state(&[Some(OWN)]);
        setup.fail_send = Some((2, Err("Enter acknowledgment lost".into())));
        let spy = SpyGuard::install(setup);
        let error = submit(entry, &session, None).unwrap_err();
        assert!(!is_prompt_ready_timeout_error(&error));
        let calls = spy.calls();
        assert_eq!(
            calls.iter().filter(|c| *c == "keys:Enter").count(),
            1,
            "{entry:?}: {calls:?}"
        );
        assert_eq!(calls.last().unwrap(), "keys:Enter", "{calls:?}");
        assert_no_cleanup(&calls);
    }
}

#[test]
fn claude_actual_entries_do_not_classify_ambiguous_send_errors_as_pre_submit_timeouts() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for entry in ENTRIES {
        for failed_send in [0, 1, 2] {
            let session = format!("claude-guard-timeout-{}", uuid::Uuid::new_v4());
            let mut setup = state(&[Some(OWN)]);
            setup.fail_send = Some((
                failed_send,
                Err("timeout waiting for claude tui follow-up prompt input readiness after 45s; previous_tui_turn_still_running=true".into()),
            ));
            let spy = SpyGuard::install(setup);
            let error = submit(entry, &session, None).unwrap_err();
            assert!(
                error.starts_with("claude tui input held after mutation:"),
                "{entry:?}: {error}"
            );
            assert!(!is_prompt_ready_timeout_error(&error), "{entry:?}: {error}");
            let calls = spy.calls();
            assert_eq!(
                calls.iter().filter(|c| *c == "keys:Enter").count(),
                usize::from(failed_send == 2)
            );
            assert_no_cleanup(&calls);
        }
    }
}

#[test]
fn claude_actual_token_entries_stop_after_enter_cancellation_without_confirmation() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for entry in [Entry::Fresh, Entry::Followup, Entry::ProvenWarm] {
        let session = format!("claude-guard-cancel-{}", uuid::Uuid::new_v4());
        let token = std::sync::Arc::new(CancelToken::new());
        let mut setup = state(&[Some(OWN), Some(BUSY)]);
        setup.cancel_on = Some(("keys:Enter", 1, token.clone()));
        let spy = SpyGuard::install(setup);
        assert_eq!(
            submit(entry, &session, Some(token.as_ref())),
            Err(PROMPT_READY_CANCELLED_ERROR.to_string()),
            "{entry:?}"
        );
        let calls = spy.calls();
        assert_eq!(calls.iter().filter(|c| *c == "keys:Enter").count(), 1);
        assert_eq!(calls.last().unwrap(), "keys:Enter", "{calls:?}");
        assert_no_cleanup(&calls);
    }
}

#[test]
fn claude_actual_fresh_entry_withholds_enter_for_unattested_eighty_column_wrap() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let session = format!("claude-guard-wrap-{}", uuid::Uuid::new_v4());
    let prompt = "[User: 명령봇 (ID: 1479017284805722200)] 응답에 정확히 한 줄로 \
                  [E2E:E50:run:AFTER_CLEAR] 만 출력해줘.";
    let rule = "─".repeat(80);
    let banner = " ▐▛███▛█   Claude Code v2.1.289\n~/.adk/release/workspaces/e2e\n\n";
    let footer = format!("{rule}\n  ⏱ 0m │ ░░░░░░░░░░ │ 0% │ 0/1.0M │ $0.00\n  MCP: 2");
    let empty = format!("{banner}{rule}\n❯ \n{footer}");
    // This observed composer wrapped a single literal line; the transport has no width proof.
    let wrapped = format!(
        "{banner}{rule}\n❯ [User: 명령봇 (ID: 1479017284805722200)] 응답에 정확히 한 줄로\n  \
         [E2E:E50:run:AFTER_CLEAR] 만\n  출력해줘.\n\n{footer}"
    );
    let spy = SpyGuard::install(SpyState {
        captures: std::iter::repeat_n(Some(empty), 16).collect(),
        captures_after_send: Some([Some(wrapped)].into()),
        ..SpyState::default()
    });
    let token = CancelToken::new();
    let error = send_fresh_prompt(&session, prompt, Some(&token)).unwrap_err();
    assert_late_hold(&error);
    let calls = spy.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|c| *c == &format!("literal:{prompt}"))
            .count(),
        1
    );
    assert!(!calls.iter().any(|c| c == "keys:Enter"), "{calls:?}");
    assert_no_cleanup(&calls);
}
