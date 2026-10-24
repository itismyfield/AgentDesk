use super::*;
use crate::services::claude_tui::composer_lock::draft_guarded;

const CAPTURE: &str =
    include_str!("../../../../../tests/fixtures/tui_input/claude-2.1.309-folded-paste.ansi");
const SHORT: &str =
    include_str!("../../../../../tests/fixtures/tui_input/claude-2.1.309-short-paste.txt");

fn long_text() -> String {
    std::iter::repeat_n(
        "Inspect this file and preserve the existing input ownership checks.",
        14,
    )
    .collect::<Vec<_>>()
    .join("\n")
}

fn folded_tui(footer: &str) -> Tui {
    let tui = Tui::new("");
    tui.put("attached", "1");
    tui.put("fold", "");
    tui.put("next-footer", &format!("{footer}\n"));
    tui.put("human.after-paste", "cp \"$d/next-footer\" \"$d/footer\"");
    tui
}

#[test]
fn folded_paste_footer_measured_capture_reads_chip_and_ownership() {
    let text = frame("iMessage", "200", "ann", NONCE, &long_text());
    assert_eq!(text.matches('\n').count(), 14);
    let shown = screen::read(CAPTURE);
    assert_eq!(
        shown.composer,
        Composer::Text(vec!["[Pasted text #1 +14 lines]".to_string()])
    );
    assert_eq!(shown.stash, Stash::AbsentInRecognizedLayout);
    assert!(screen::owns_direct(CAPTURE, &text, &screen::Drawn::Folded));
}

#[test]
fn folded_paste_footer_submits_once_without_a_draft_guard() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let footer = CAPTURE.lines().skip(4).collect::<Vec<_>>().join("\n");
    let tui = folded_tui(&footer);
    let got = tui.run_text(&long_text(), &FAST);
    let guarded = draft_guarded(&tui.session());
    assert_eq!(
        got,
        report(Outcome::Injected, DraftState::Unchanged),
        "guard={guarded}; applied={:?}",
        tui.applied()
    );
    assert_eq!(tui.applied(), ["paste", "Enter"]);
    assert!(tui.taken());
    assert!(!guarded);
    assert!(tui.alerts().is_empty());
}

#[test]
fn folded_paste_footer_other_advice_remains_unreadable_and_unsubmitted() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for footer in [
        "  paste again to expand later",
        "  paste again to expand · esc to interrupt",
        "  paste again to expan",
        "  Paste again to expand",
        " paste again to expand",
    ] {
        let tui = folded_tui(footer);
        assert_eq!(
            tui.run_text(&long_text(), &FAST),
            report(
                Outcome::Unconfirmed(Unconfirmed::DraftNotOwned),
                DraftState::Unknown
            ),
            "{footer}"
        );
        assert_eq!(
            screen::read(&tui.capture()).composer,
            Composer::Unknown,
            "{footer}"
        );
        assert_eq!(tui.applied(), ["paste"], "{footer}");
        assert!(tui.records().is_empty(), "{footer}");
        assert!(draft_guarded(&tui.session()), "{footer}");
    }
}

#[test]
fn folded_paste_footer_short_frame_keeps_the_unfolded_path() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let text = SHORT.trim_end();
    assert_eq!(text.matches('\n').count(), 1);
    assert!(matches!(
        screen::drawn(text, Some((160, 50))),
        Some(screen::Drawn::Rows(_))
    ));
    let tui = Tui::new("");
    tui.put("attached", "1");
    let session = tui.session();
    let nonce = tui.next_nonce();
    let pane = Pane::with_program(&session, tui.dir.path().join("tmux"));
    let request = Request {
        session: &session,
        transcript: &tui.transcript,
        source: "iMessage",
        author: "343742347365974026",
        name: "0hbujang",
        nonce: &nonce,
        text,
    };
    let got = crate::services::claude_tui::composer_lock::try_with_composer_mutation_lock(
        &session,
        || inject_locked(&pane, &request, text, &FAST, true, TurnGate::BusyOnly),
    )
    .unwrap();
    assert_eq!(got, report(Outcome::Injected, DraftState::Unchanged));
    assert_eq!(tui.applied(), ["paste", "Enter"]);
    assert!(tui.taken());
    assert!(!draft_guarded(&tui.session()));
}
