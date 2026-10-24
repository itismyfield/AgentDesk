use super::*;
use crate::services::claude_tui::composer_lock::DraftSighting;

const MEASURED: &str =
    include_str!("../../../../../tests/fixtures/tui_input/claude-2.1.309-folded-paste.ansi");
const MODAL: &str = "  ❯ 1. Resume from summary (recommended)\n    2. Resume full session as-is\n  Enter to confirm · Esc to cancel\n";

fn long_text() -> String {
    std::iter::repeat_n(
        "Inspect the full input while preserving the person's existing draft.",
        14,
    )
    .collect::<Vec<_>>()
    .join("\n")
}

fn measured_status(tui: &Tui) {
    tui.put("width", "114");
    tui.put("height", "60");
    tui.put("border", &format!("{}\n", "─".repeat(114)));
    tui.put(
        "effortrow",
        &format!("{}\n", MEASURED.lines().next().unwrap()),
    );
    tui.put("stashrow", &format!("{:>112}\n", "› stashed"));
}

#[test]
fn a1_multiline_stash_preserves_plain_and_folded_human_bytes_on_measured_status() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for folded_draft in [false, true] {
        let draft = if folded_draft {
            std::iter::repeat_n("  original human text — 한글", 14)
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            "first human line\n\n  indented third line — 한글".to_string()
        };
        let tui = Tui::new(&draft);
        tui.put("attached", "1");
        tui.put("fold", "");
        measured_status(&tui);
        let chip = "[Pasted text #7 +13 lines]";
        if folded_draft {
            tui.put("chip", chip);
        }
        assert_eq!(
            screen::read(&tui.capture()).stash,
            Stash::AbsentInRecognizedLayout
        );
        assert_eq!(
            composer_owner_in_session(&tui.session(), &tui.capture()),
            ComposerOwner::Person,
            "an unrelated human chip must remain a person's draft"
        );
        assert_eq!(
            tui.run_text(&long_text(), &FAST),
            report(Outcome::Injected, DraftState::RestoredObserved)
        );
        assert_eq!(tui.applied(), ["C-s", "paste", "Enter"]);
        assert_eq!(tui.drafts(), (draft, None));
        assert_eq!(tui.get("chip").as_deref(), folded_draft.then_some(chip));
        assert!(
            tui.taken(),
            "receipt must contain the raw payload, not its rendered chip"
        );
        assert_eq!(tui.records(), [tui.get("buffer").unwrap()]);
        assert!(tui.alerts().is_empty());
    }
}

#[test]
fn a1_stash_accepts_its_folded_attempt_footer_and_restores_the_person() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let draft = "human draft\n\n  preserve indentation";
    let tui = Tui::new(draft);
    tui.put("attached", "1");
    tui.put("fold", "");
    tui.put("next-footer", "  paste again to expand\n");
    tui.put("normal-footer", &tui.get("footer").unwrap());
    tui.put("human.after-paste", "cp \"$d/next-footer\" \"$d/footer\"");
    tui.put(
        "human.before-Enter",
        "cp \"$d/normal-footer\" \"$d/footer\"",
    );
    assert_eq!(
        tui.run_text(&long_text(), &FAST),
        report(Outcome::Injected, DraftState::RestoredObserved)
    );
    assert_eq!(tui.applied(), ["C-s", "paste", "Enter"]);
    assert_eq!(tui.drafts(), (draft.to_string(), None));
    assert!(tui.taken());
}

#[test]
fn a1_paste_footer_never_admits_an_unowned_person_draft() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for folded_draft in [false, true] {
        let tui = Tui::new("the person's original bytes\nsecond line");
        tui.put("attached", "1");
        if folded_draft {
            tui.put("chip", "[Pasted text #8 +1 lines]");
        }
        tui.put("footer", "  paste again to expand\n");
        let before = tui.drafts();
        let capture = tui.capture();
        assert_eq!(
            draft_sighting_in_session(&tui.session(), &capture),
            DraftSighting::Unsettled
        );
        assert_eq!(
            composer_owner_in_session(&tui.session(), &capture),
            ComposerOwner::Unread
        );
        assert_eq!(
            tui.run(),
            report(Outcome::NotSent(Veto::Draft), DraftState::Unchanged)
        );
        assert!(tui.applied().is_empty());
        assert!(tui.records().is_empty());
        assert_eq!(tui.drafts(), before);
    }
}

#[test]
fn a1_busy_modal_words_in_composer_are_stashed_but_dialog_chrome_never_gets_keys() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let draft = "Please explain Enter to confirm and Esc to cancel as ordinary words.";
    for dialog in [false, true] {
        let tui = Tui::new(draft);
        tui.put("attached", "1");
        if dialog {
            tui.put("head", &format!("{MODAL}{}", tui.get("head").unwrap()));
        }
        assert_eq!(
            tui.run(),
            if dialog {
                report(Outcome::NotSent(Veto::Modal), DraftState::Unchanged)
            } else {
                report(Outcome::Injected, DraftState::RestoredObserved)
            },
            "dialog={dialog}"
        );
        assert_eq!(
            tui.applied(),
            if dialog {
                vec![]
            } else {
                vec!["C-s", "paste", "Enter"]
            }
        );
        assert_eq!(tui.drafts(), (draft.to_string(), None));
        assert_eq!(tui.taken(), !dialog);
    }
}

#[test]
fn a1_busy_modal_appearing_after_stash_or_paste_stops_before_the_next_key() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for after in ["C-s", "paste"] {
        let tui = Tui::new("human draft");
        tui.put("attached", "1");
        tui.put("modal", MODAL);
        tui.put(
            &format!("human.after-{after}"),
            "cat \"$d/modal\" > \"$d/head\"",
        );
        let outcome = if after == "C-s" {
            Outcome::NotSent(Veto::Modal)
        } else {
            Outcome::Unconfirmed(Unconfirmed::ModalAfterPaste)
        };
        assert_eq!(tui.run(), report(outcome, DraftState::Unknown), "{after}");
        assert_eq!(
            tui.applied(),
            if after == "C-s" {
                vec!["C-s"]
            } else {
                vec!["C-s", "paste"]
            }
        );
        assert!(tui.records().is_empty());
        assert_eq!(tui.get("stash").as_deref(), Some("human draft"));
    }
}

#[test]
fn a1_our_folded_attempt_residue_is_unread_not_a_person() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let tui = Tui::new("");
    tui.put("attached", "1");
    tui.put("fold", "");
    tui.person_after_capture(2, "echo 999 > \"$d/cursor_x\"");
    assert_eq!(
        tui.run_text(&long_text(), &FAST),
        report(
            Outcome::Unconfirmed(Unconfirmed::CursorChangedBeforeEnter),
            DraftState::Unknown
        )
    );
    assert_eq!(tui.applied(), ["paste"]);
    assert!(tui.records().is_empty());
    assert_eq!(tui.get("composer"), tui.get("buffer"));
    let capture = tui.capture();
    assert_eq!(
        composer_owner_in_session(&tui.session(), &capture),
        ComposerOwner::Unread
    );
    assert_ne!(
        draft_sighting_in_session(&tui.session(), &capture),
        DraftSighting::PersonDraft
    );
}
