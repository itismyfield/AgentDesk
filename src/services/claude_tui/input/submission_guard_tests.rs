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
    submit_text(entry, session, PROMPT, token)
}

fn submit_text(
    entry: Entry,
    session: &str,
    prompt: &str,
    token: Option<&CancelToken>,
) -> Result<(), String> {
    // Hosted dispatch supplies a token; its polling path stays on the thread owning this spy.
    let local = CancelToken::new();
    let token = Some(token.unwrap_or(&local));
    match entry {
        Entry::Fresh => send_fresh_prompt(session, prompt, token),
        Entry::Followup => send_followup_prompt(session, prompt, token),
        Entry::Steering => inject_steering_prompt(session, prompt),
        Entry::ProvenWarm => {
            let transcript = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(
                transcript.path(),
                "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
            )
            .unwrap();
            send_followup_prompt_or_idle_transcript(session, prompt, token, transcript.path())
        }
    }
}

fn state(after: &[Option<&str>]) -> SpyState {
    SpyState {
        pane_size: Some((80, 24)),
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

// These attributes, 120-column borders and footer were captured from Claude's idle composer.
fn measured_composer(row: &str) -> String {
    let border = format!("\x1b[38;5;244m{}", "─".repeat(120));
    let footer = "\x1b[39m  \x1b[38;5;211m⏵⏵ bypass permissions on\x1b[38;5;246m (shift+tab to cycle)\x1b[39m";
    format!("⏺ Done.\n\n\n{border}\n{row}\n{border}\n{footer}\n")
}

#[test]
fn claude_actual_idle_entries_withhold_enter_when_native_empty_hint_matches_prompt_text() {
    use crate::services::tui_input::actor::gate::{own_draft, own_wrapped_draft};
    use crate::services::tui_o::shadow::ShadowProvider;
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let prompt = "Try \"refactor <filepath>\"";
    let empty = measured_composer(&format!("\x1b[39m❯\u{a0}\x1b[2m{prompt}\x1b[0m"));
    let stashed = empty.replacen("⏺ Done.\n\n\n", "⏺ Done.\n\n› stashed\n", 1);
    let typed = measured_composer(&format!("\x1b[39m❯\u{a0}{prompt}"));
    assert_eq!(
        crate::services::claude_tui::busy_inject::draft_sighting(&empty),
        crate::services::claude_tui::composer_lock::DraftSighting::Settled
    );
    assert_eq!(
        crate::services::claude_tui::busy_inject::draft_sighting(&stashed),
        crate::services::claude_tui::composer_lock::DraftSighting::Unsettled
    );
    assert!(own_draft(ShadowProvider::Claude, &typed, prompt, true));
    assert!(own_wrapped_draft(&typed, &[prompt.to_string()]));
    for after in [&empty, &stashed] {
        for entry in [Entry::Fresh, Entry::Followup, Entry::ProvenWarm] {
            let session = format!("claude-guard-hint-alias-{}", uuid::Uuid::new_v4());
            let spy = SpyGuard::install(state(&[Some(after), Some(BUSY)]));
            let result = submit_text(entry, &session, prompt, None);
            let calls = spy.calls();
            assert_eq!(
                calls
                    .iter()
                    .filter(|c| *c == &format!("literal:{prompt}"))
                    .count(),
                1,
                "{entry:?}: {result:?}: {calls:?}"
            );
            assert_eq!(
                calls.iter().filter(|c| *c == "keys:Enter").count(),
                0,
                "{entry:?}: {result:?}: {calls:?}"
            );
            assert_late_hold(&result.unwrap_err());
            assert_no_cleanup(&calls);
        }
        assert!(!own_draft(ShadowProvider::Claude, after, prompt, true));
        assert!(!own_wrapped_draft(after, &[prompt.to_string()]));
    }
}

#[test]
fn claude_actual_idle_entries_preserve_measured_faint_empty_composer_semantics() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let hint = "\x1b[39m❯\u{a0}\x1b[2mTry \"refactor <filepath>\"\x1b[0m";
    let empty = measured_composer(hint);
    assert_eq!(
        crate::services::claude_tui::busy_inject::draft_sighting(&empty),
        crate::services::claude_tui::composer_lock::DraftSighting::Settled
    );
    let invalid = [
        measured_composer("\x1b[39m❯\u{a0}Try \"refactor <filepath>\""),
        measured_composer("\x1b[39m❯\u{a0}\x1b[2mTry \"x\"\x1b[22m"),
        measured_composer("\x1b[39m❯ \x1b[2mTry \"x\"\x1b[0m"),
        measured_composer("\x1b[39m❯\u{a0}\x1b[2m\x1b[38;5;246mTry \"x\"\x1b[0m"),
        measured_composer("\x1b[39m❯\u{a0}/he\x1b[2mlp\x1b[0m"),
        measured_composer("\x1b[39m❯\u{a0}\x1b[2mTry \"x\"\x1b[0m\x1b[2m more\x1b[0m"),
        measured_composer(&format!("{hint}\n  \x1b[2mTry \"another file\"\x1b[0m")),
        measured_composer(&format!("{hint}\n  foreign continuation")),
        measured_composer(&format!("  {hint}")),
        measured_composer("❯\u{a0}[User: A (ID:1)] stranded"),
    ];
    for entry in [Entry::Fresh, Entry::Followup, Entry::ProvenWarm] {
        for changed in &invalid {
            let session = format!("claude-guard-faint-pre-{}", uuid::Uuid::new_v4());
            let mut setup = state(&[Some(OWN), Some(BUSY)]);
            let draft_read = if matches!(entry, Entry::Fresh) { 1 } else { 2 };
            setup.draft_capture_at = Some((draft_read, Some(changed.clone())));
            let spy = SpyGuard::install(setup);
            let result = submit(entry, &session, None);
            let calls = spy.calls();
            assert!(
                !calls
                    .iter()
                    .any(|call| ["literal:", "load:", "paste:", "keys:"]
                        .iter()
                        .any(|prefix| call.starts_with(prefix))),
                "{entry:?}: {result:?}: {changed:?}: {calls:?}"
            );
            assert!(
                result.is_err_and(
                    |error| error.starts_with("claude tui input refused before mutation:")
                ),
                "{entry:?}: {changed:?}"
            );
            assert_no_cleanup(&calls);
        }
        for confirmation in [&*empty, BUSY] {
            let session = format!("claude-guard-faint-owned-{}", uuid::Uuid::new_v4());
            let mut setup = state(&[Some(OWN), Some(confirmation)]);
            setup.captures = std::iter::repeat_n(Some(empty.clone()), 16).collect();
            let spy = SpyGuard::install(setup);
            let result = submit(entry, &session, None);
            let calls = spy.calls();
            assert_eq!(result, Ok(()), "{entry:?}: {calls:?}");
            assert_eq!(
                calls.iter().filter(|c| *c == "paste:delete=true").count(),
                1
            );
            assert_eq!(calls.iter().filter(|c| *c == "keys:Enter").count(), 1);
            let paste = calls.iter().position(|c| c == "paste:delete=true").unwrap();
            let enter = calls.iter().position(|c| c == "keys:Enter").unwrap();
            assert_eq!(&calls[paste + 1..enter], ["size", "alive", "capture:draft"]);
            assert_eq!(&calls[enter + 1..], ["capture:draft", "alive"]);
            assert_no_cleanup(&calls);
        }
    }
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
            assert_eq!(&calls[paste + 1..enter], ["size", "alive", "capture:draft"]);
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
            let spy = SpyGuard::install(state(&[
                Some(OWN),
                confirmation,
                confirmation,
                confirmation,
            ]));
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
                3,
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
fn claude_actual_fresh_entry_submits_observed_eighty_column_wrap_once() {
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
    // This observed composer wrapped one literal line; the whole visible body must match.
    let wrapped = format!(
        "{banner}{rule}\n❯ [User: 명령봇 (ID: 1479017284805722200)] 응답에 정확히 한 줄로\n  \
         [E2E:E50:run:AFTER_CLEAR] 만\n  출력해줘.\n\n{footer}"
    );
    let spy = SpyGuard::install(SpyState {
        pane_size: Some((80, 24)),
        captures: std::iter::repeat_n(Some(empty), 16).collect(),
        captures_after_send: Some([Some(wrapped), Some(BUSY.to_string())].into()),
        ..SpyState::default()
    });
    let token = CancelToken::new();
    assert_eq!(send_fresh_prompt(&session, prompt, Some(&token)), Ok(()));
    let calls = spy.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|c| *c == &format!("literal:{prompt}"))
            .count(),
        1
    );
    assert_eq!(
        calls.iter().filter(|c| *c == "keys:Enter").count(),
        1,
        "{calls:?}"
    );
    assert_no_cleanup(&calls);
}

fn constructed_composer(width: usize, body: &str) -> String {
    let border = "─".repeat(width);
    let shown = body
        .split('\n')
        .enumerate()
        .map(|(i, row)| {
            if i == 0 {
                format!("❯ {row}")
            } else if row.is_empty() {
                String::new()
            } else {
                format!("  {row}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "⏺ Done.\n\n\n{border}\n{shown}\n{border}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n"
    )
}

#[test]
fn claude_actual_entries_submit_wrapped_whole_prompts_once() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let korean = format!(
        "[User: 에이전트데스크봇 (ID: 1479017284805722200)] {}",
        "가".repeat(40)
    );
    let plain = format!("plain-{}", "x".repeat(160));
    let cases = [
        (
            (80, 24),
            &*korean,
            include_str!(
                "../../../../tests/fixtures/tui_input/claude-constructed-korean-wrap-80.txt"
            ),
        ),
        (
            (46, 37),
            &*korean,
            include_str!(
                "../../../../tests/fixtures/tui_input/claude-constructed-korean-wrap-46.txt"
            ),
        ),
        (
            (125, 39),
            &*korean,
            include_str!(
                "../../../../tests/fixtures/tui_input/claude-constructed-korean-wrap-125.txt"
            ),
        ),
        (
            (80, 24),
            &*plain,
            include_str!(
                "../../../../tests/fixtures/tui_input/claude-constructed-plain-wrap-80.txt"
            ),
        ),
        (
            (46, 37),
            &*plain,
            include_str!(
                "../../../../tests/fixtures/tui_input/claude-constructed-plain-wrap-46.txt"
            ),
        ),
        (
            (125, 39),
            &*plain,
            include_str!(
                "../../../../tests/fixtures/tui_input/claude-constructed-plain-wrap-125.txt"
            ),
        ),
    ];
    for entry in ENTRIES {
        for ((width, height), prompt, owned) in cases {
            let ending = format!("\n{}\n  ⏵⏵", "─".repeat(width));
            let foreign = owned.replacen(&ending, &format!(" 사람이붙인문자{ending}"), 1);
            assert_ne!(foreign, owned);
            for after in [owned, &*foreign] {
                let session = format!("claude-r3-wrap-{}", uuid::Uuid::new_v4());
                let mut setup = state(&[Some(after), Some(BUSY)]);
                let empty = constructed_composer(width, "");
                setup.captures = std::iter::repeat_n(Some(empty), 16).collect();
                setup.pane_size = Some((width, height));
                let spy = SpyGuard::install(setup);
                let result = submit_text(entry, &session, prompt, None);
                let calls = spy.calls();
                assert_eq!(
                    calls
                        .iter()
                        .filter(|c| *c == &format!("literal:{prompt}"))
                        .count(),
                    1,
                    "{calls:?}"
                );
                assert_eq!(
                    calls.iter().filter(|c| *c == "keys:Enter").count(),
                    usize::from(after == owned),
                    "{entry:?}: width={width}: {result:?}: {calls:?}"
                );
                if after == owned {
                    assert_eq!(result, Ok(()));
                    let enter = calls.iter().position(|c| c == "keys:Enter").unwrap();
                    assert_eq!(&calls[enter - 3..enter], ["size", "alive", "capture:draft"]);
                } else {
                    assert_late_hold(&result.unwrap_err());
                }
                assert_no_cleanup(&calls);
            }
        }
    }
}

#[test]
fn claude_actual_entries_submit_modal_words_in_body_and_history_once() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for entry in ENTRIES {
        for prompt in [
            "settings.json allow 목록에 X 넣고 deny 에서 Y 빼줘",
            "왜 rejected 됐는지 allowed 목록 확인해줘",
            "Enter 키로 select 하는 방법을 설명해줘",
            "✳ Architecting…",
            "• Working (1s • esc to interrupt)",
            "다음 화면을 설명해줘\n✳ Architecting…",
            "다음 화면을 설명해줘\n• Working (1s • esc to interrupt)",
        ] {
            let session = format!("claude-r3-modal-body-{}", uuid::Uuid::new_v4());
            let owned = constructed_composer(125, prompt);
            let mut setup = state(&[Some(&owned), Some(BUSY)]);
            let empty = constructed_composer(125, "");
            setup.captures = std::iter::repeat_n(Some(empty), 16).collect();
            setup.pane_size = Some((125, 39));
            let spy = SpyGuard::install(setup);
            assert_eq!(
                submit_text(entry, &session, prompt, None),
                Ok(()),
                "{entry:?}"
            );
            let calls = spy.calls();
            assert_eq!(calls.iter().filter(|c| *c == "keys:Enter").count(), 1);
            assert_no_cleanup(&calls);
        }
    }
    // Steering has separate modal admission; normal turn entries accept separated history.
    for entry in [Entry::Fresh, Entry::Followup, Entry::ProvenWarm] {
        for history in [
            "⏺ allow this setting and reject that setting",
            "❯ Enter to select · Esc to cancel was quoted in the previous turn\n⏺ Here is how that interface works.",
            "⏺ Sign in instructions:\n  1. Open settings\n  2. Select the account",
        ] {
            let session = format!("claude-r3-modal-history-{}", uuid::Uuid::new_v4());
            let before = format!("{history}\n\n{IDLE}");
            let mut setup = state(&[Some(OWN), Some(BUSY)]);
            setup.captures = std::iter::repeat_n(Some(before), 16).collect();
            let spy = SpyGuard::install(setup);
            assert_eq!(submit(entry, &session, None), Ok(()), "{entry:?}");
            let calls = spy.calls();
            assert_eq!(calls.iter().filter(|c| *c == "keys:Enter").count(), 1);
            assert_no_cleanup(&calls);
        }
    }
}

#[test]
fn claude_actual_entries_withhold_enter_for_current_permission_chrome() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let footer_permission =
        format!("{OWN}Allow this command\nReject this command\nEnter to confirm · Esc to cancel\n");
    let above_permission = OWN.replacen(
        "────────────────────\n❯",
        "Allow this command\nReject this command\n────────────────────\n❯",
        1,
    );
    assert_ne!(above_permission, OWN);
    for entry in ENTRIES {
        for after in [MODAL, &*footer_permission, &*above_permission] {
            assert_eq!(
                crate::services::tui_input::actor::gate::judge_pane(
                    crate::services::tui_o::shadow::ShadowProvider::Claude,
                    after,
                ),
                crate::services::tui_input::actor::gate::PaneVerdict::Modal,
            );
            let session = format!("claude-r3-current-modal-{}", uuid::Uuid::new_v4());
            let spy = SpyGuard::install(state(&[Some(after)]));
            let error = submit(entry, &session, None).unwrap_err();
            assert_late_hold(&error);
            let calls = spy.calls();
            assert_eq!(calls.iter().filter(|c| *c == "keys:Enter").count(), 0);
            assert_no_cleanup(&calls);
        }
    }
}

#[test]
fn claude_actual_entries_passively_wait_for_three_readonly_repaints() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for entry in ENTRIES {
        for final_capture in [BUSY, OWN] {
            let session = format!("claude-r3-passive-{}", uuid::Uuid::new_v4());
            let spy = SpyGuard::install(state(&[
                Some(OWN),
                Some(OWN),
                Some(OWN),
                Some(final_capture),
            ]));
            let result = submit(entry, &session, None);
            let calls = spy.calls();
            if final_capture == BUSY {
                assert_eq!(result, Ok(()), "{entry:?}: {calls:?}");
            } else {
                assert_late_hold(&result.unwrap_err());
            }
            assert_eq!(calls.iter().filter(|c| *c == "keys:Enter").count(), 1);
            let enter = calls.iter().position(|c| c == "keys:Enter").unwrap();
            assert_eq!(
                calls[enter + 1..]
                    .iter()
                    .filter(|c| *c == "capture:draft")
                    .count(),
                3,
                "{calls:?}"
            );
            assert!(
                calls[enter + 1..]
                    .iter()
                    .all(|c| c == "capture:draft" || c == "alive"),
                "{calls:?}"
            );
            assert_no_cleanup(&calls);
        }
    }
}

#[test]
fn claude_actual_token_entries_stop_passive_poll_on_cancellation() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for entry in [Entry::Fresh, Entry::Followup, Entry::ProvenWarm] {
        let session = format!("claude-r3-passive-cancel-{}", uuid::Uuid::new_v4());
        let token = std::sync::Arc::new(CancelToken::new());
        let mut setup = state(&[Some(OWN), Some(OWN), Some(BUSY)]);
        let passive_read = if matches!(entry, Entry::Fresh) { 3 } else { 4 };
        setup.cancel_on = Some(("capture:draft", passive_read, token.clone()));
        let spy = SpyGuard::install(setup);
        assert_eq!(
            submit(entry, &session, Some(token.as_ref())),
            Err(PROMPT_READY_CANCELLED_ERROR.to_string())
        );
        let calls = spy.calls();
        assert_eq!(calls.iter().filter(|c| *c == "keys:Enter").count(), 1);
        let enter = calls.iter().position(|c| c == "keys:Enter").unwrap();
        assert_eq!(
            calls[enter + 1..]
                .iter()
                .filter(|c| *c == "capture:draft")
                .count(),
            1
        );
        assert!(
            calls[enter + 1..]
                .iter()
                .all(|c| c == "capture:draft" || c == "alive"),
            "{calls:?}"
        );
        assert_no_cleanup(&calls);
    }
}

#[test]
fn claude_actual_entries_submit_large_multiline_native_bfold_with_exact_lf_count() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let prompt = format!(
        "첫째{}\n둘째{}\n셋째{}\n넷째{}",
        "가".repeat(300),
        "나".repeat(300),
        "다".repeat(300),
        "라".repeat(300),
    );
    assert!(prompt.chars().count() > 1000);
    assert_eq!(prompt.bytes().filter(|b| *b == b'\n').count(), 3);
    let empty = constructed_composer(80, "");
    let folded = constructed_composer(80, "[Pasted text #19 +3 lines]");
    let wrong_lf_count = constructed_composer(80, "[Pasted text #19 +4 lines]");
    let codex_count_only = constructed_composer(
        80,
        &format!("[Pasted Content {} chars]", prompt.chars().count()),
    );
    for entry in ENTRIES {
        for after in [&folded, &wrong_lf_count, &codex_count_only] {
            let session = format!("claude-r3-large-bfold-{}", uuid::Uuid::new_v4());
            let mut setup = state(&[Some(after), Some(BUSY)]);
            setup.captures = std::iter::repeat_n(Some(empty.clone()), 16).collect();
            setup.pane_size = Some((80, 24));
            let spy = SpyGuard::install(setup);
            let result = submit_text(entry, &session, &prompt, None);
            let calls = spy.calls();
            assert_eq!(
                calls
                    .iter()
                    .filter(|c| *c == &format!("load:{prompt}"))
                    .count(),
                1,
                "{entry:?}: {calls:?}",
            );
            assert_eq!(
                calls.iter().filter(|c| *c == "paste:delete=true").count(),
                1
            );
            assert_eq!(
                calls.iter().filter(|c| *c == "keys:Enter").count(),
                usize::from(after == &folded),
                "{entry:?}: {result:?}: {calls:?}",
            );
            if after == &folded {
                assert_eq!(result, Ok(()));
            } else {
                assert_late_hold(&result.unwrap_err());
            }
            assert_no_cleanup(&calls);
        }
    }
}

#[test]
fn claude_actual_entries_deliver_long_single_line_as_one_folded_paste() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let empty = constructed_composer(80, "");
    let folded = constructed_composer(80, "[Pasted text #3]");
    let wrong_lf_count = constructed_composer(80, "[Pasted text #3 +1 lines]");
    let lines = [
        "가".repeat(801),
        "가".repeat(1200),
        format!("{}x", "x ".repeat(999)),
    ];
    for prompt in lines {
        // Typed as rows, each line needs more than the 14 composer rows of 80x24.
        assert!(prompt.chars().count() > 800 && !prompt.contains('\n'));
        for entry in ENTRIES {
            for after in [&folded, &wrong_lf_count] {
                let session = format!("claude-r4-long-line-{}", uuid::Uuid::new_v4());
                let mut setup = state(&[Some(after), Some(BUSY)]);
                setup.captures = std::iter::repeat_n(Some(empty.clone()), 16).collect();
                setup.pane_size = Some((80, 24));
                let spy = SpyGuard::install(setup);
                let result = submit_text(entry, &session, &prompt, None);
                let calls = spy.calls();
                let count = |name: &str| calls.iter().filter(|c| *c == name).count();
                assert_eq!(count(&format!("load:{prompt}")), 1, "{entry:?}: {calls:?}");
                assert_eq!(count("paste:delete=true"), 1, "{entry:?}: {calls:?}");
                assert!(
                    !calls.iter().any(|c| c.starts_with("literal:")),
                    "{calls:?}"
                );
                assert_eq!(
                    count("keys:Enter"),
                    usize::from(after == &folded),
                    "{entry:?}: {result:?}: {calls:?}",
                );
                if after == &folded {
                    assert_eq!(result, Ok(()), "{entry:?}");
                } else {
                    assert_late_hold(&result.unwrap_err());
                }
                assert_no_cleanup(&calls);
            }
        }
    }
    // At 800 chars Claude does not fold, so the overflowing rows stay a typed pre-effect refusal.
    let unfoldable = "가".repeat(800);
    for entry in ENTRIES {
        let session = format!("claude-r4-unfoldable-{}", uuid::Uuid::new_v4());
        let mut setup = state(&[Some(BUSY)]);
        setup.captures = std::iter::repeat_n(Some(empty.clone()), 16).collect();
        setup.pane_size = Some((80, 24));
        let spy = SpyGuard::install(setup);
        let result = submit_text(entry, &session, &unfoldable, None);
        let calls = spy.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("literal:")
                || c.starts_with("load:")
                || c.starts_with("paste:")
                || c.starts_with("keys:")),
            "{entry:?}: {result:?}: {calls:?}",
        );
        let error = result.unwrap_err();
        assert!(
            error.starts_with("claude tui input refused before mutation:"),
            "{entry:?}: {error}",
        );
        assert!(error.contains("Composer(UnpredictableRender)"), "{error}");
        assert!(!is_prompt_ready_timeout_error(&error), "{error}");
        assert_no_cleanup(&calls);
    }
}

#[test]
fn claude_actual_entries_withhold_enter_when_geometry_changes_after_payload() {
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for entry in ENTRIES {
        for after_send in [None, Some(Some((46, 37))), Some(None)] {
            let session = format!("claude-r4-resized-{}", uuid::Uuid::new_v4());
            let mut setup = state(&[Some(OWN), Some(BUSY)]);
            setup.pane_size_after_send = after_send;
            let spy = SpyGuard::install(setup);
            let result = submit(entry, &session, None);
            let calls = spy.calls();
            assert_eq!(
                calls.iter().filter(|c| *c == "paste:delete=true").count(),
                1
            );
            assert_eq!(
                calls.iter().filter(|c| *c == "keys:Enter").count(),
                usize::from(after_send.is_none()),
                "{entry:?}: {after_send:?}: {result:?}: {calls:?}",
            );
            if after_send.is_none() {
                assert_eq!(result, Ok(()), "{entry:?}");
            } else {
                // The own draft captured from another geometry cannot prove the preflight fit.
                assert_late_hold(&result.unwrap_err());
            }
            assert_no_cleanup(&calls);
        }
    }
}
