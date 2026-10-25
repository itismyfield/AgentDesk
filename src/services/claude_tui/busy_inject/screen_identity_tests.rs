use super::*;
use crate::services::claude_tui::composer_lock::DraftSighting;
use crate::services::tui_input::human_frame::{history, idle};

fn capture(frame: &str) -> String {
    format!(
        "⏺ Working.\n\n────────────────────────\n❯ {}\n────────────────────────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)",
        frame.replace('\n', "\n  ")
    )
}

#[test]
fn pending_plain_composer_protects_only_its_session_and_frame_until_receipt() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let transcript = root.path().join("native.jsonl");
    std::fs::write(&transcript, "").unwrap();
    let frame = idle("ann", "7", "please inspect\nthe next line");
    history::register(
        ("claude", "pending-screen", "screen-attempt", "7"),
        &transcript,
        0,
        &frame,
    )
    .unwrap();
    let shown = capture(&frame);
    assert_eq!(
        composer_owner_in_session("pending-screen", &shown),
        ComposerOwner::Unread,
        "an unconfirmed frame is neither a person's draft nor a cleanup-owned idle prompt"
    );
    assert_ne!(
        draft_sighting_in_session("pending-screen", &shown),
        DraftSighting::PersonDraft
    );
    for (session, body) in [
        ("other-screen", frame.as_str()),
        ("pending-screen", "[User: ann (ID: 7)] a different input"),
        ("pending-screen", "[User: ann (ID: 7)]"),
    ] {
        assert_eq!(
            composer_owner_in_session(session, &capture(body)),
            ComposerOwner::AgentDesk,
            "a header alone or another session is not our pending frame"
        );
    }
    assert_eq!(
        history::receipt(
            "screen-attempt",
            &transcript,
            0,
            "enqueue",
            Some("screen-row"),
            &frame
        ),
        Some(true)
    );
    assert_eq!(
        composer_owner_in_session("pending-screen", &shown),
        ComposerOwner::AgentDesk,
        "receipt invalidates the pending-screen cache"
    );
    assert_eq!(
        draft_sighting_in_session("pending-screen", &shown),
        DraftSighting::PersonDraft
    );
    let wrapped = frame.replace("inspect", "in\nspect");
    assert!(!history::screen_pending("pending-screen", &wrapped));
}

#[test]
fn injected_nonce_residue_is_never_classified_as_a_persons_draft() {
    for frame in [
        crate::services::tui_input::human_frame::inject(
            "discord:1:2",
            "7",
            "ann",
            "1234abcd",
            "pending body",
        ),
        crate::services::tui_input::human_frame::legacy_inject(
            "discord:1:2",
            "7",
            "1234abcd",
            "pending body",
        ),
    ] {
        let shown = capture(&frame);
        assert_eq!(composer_owner(&shown), ComposerOwner::Unread, "{frame}");
        assert_eq!(draft_sighting(&shown), DraftSighting::Unsettled, "{frame}");
    }
}

#[test]
fn observed_pending_chip_protects_only_its_session_until_the_composer_changes() {
    let session = format!("pending-chip-{}", uuid::Uuid::new_v4());
    let frame = idle("ann", "7", "line one\nline two\nline three\nline four");
    let chip = capture("[Pasted text #14 +4 lines]");
    ownership::remember_unconfirmed_chip(&session, "1234abcd", &frame, &chip);
    assert_eq!(
        composer_owner_in_session(&session, &chip),
        ComposerOwner::Unread,
        "an observed chip left by our unconfirmed paste is not a person's draft"
    );
    assert_eq!(
        draft_sighting_in_session(&session, &chip),
        DraftSighting::Unsettled
    );
    assert_eq!(
        composer_owner_in_session("other-session", &chip),
        ComposerOwner::Person
    );
    assert_eq!(
        composer_owner_in_session(&session, &capture("new human text")),
        ComposerOwner::Person
    );
    assert_eq!(
        composer_owner_in_session(&session, &chip),
        ComposerOwner::Person,
        "once different composer text was observed, the old chip does not identify a new paste"
    );
    ownership::remember_unconfirmed_chip(&session, "5678abcd", &frame, &chip);
    assert_eq!(
        composer_owner_in_session(&session, &capture("")),
        ComposerOwner::Empty
    );
    assert_eq!(
        composer_owner_in_session(&session, &chip),
        ComposerOwner::Person
    );
}

#[test]
fn pending_chip_evidence_requires_an_exact_chip_and_an_attempt_nonce() {
    let session = format!("chip-evidence-{}", uuid::Uuid::new_v4());
    let frame = idle("ann", "7", "line one\nline two\nline three\nline four");
    let chip = capture("[Pasted text #14 +4 lines]");
    for nonce in ["", "not-an-attempt"] {
        ownership::remember_unconfirmed_chip(&session, nonce, &frame, &chip);
        assert_eq!(
            composer_owner_in_session(&session, &chip),
            ComposerOwner::Person
        );
    }
    let other_chip = capture("[Pasted text #14 +3 lines]");
    ownership::remember_unconfirmed_chip(&session, "1234abcd", &frame, &other_chip);
    assert_eq!(
        composer_owner_in_session(&session, &other_chip),
        ComposerOwner::Person
    );
    ownership::remember_unconfirmed_chip(&session, "1234abcd", &frame, &chip);
    assert_eq!(
        composer_owner_in_session(&session, "unreadable capture"),
        ComposerOwner::Unread
    );
    assert_eq!(
        composer_owner_in_session(&session, &chip),
        ComposerOwner::Unread
    );
    ownership::forget_pending_chip(&session);
    assert_eq!(
        composer_owner_in_session(&session, &chip),
        ComposerOwner::Person
    );
}

#[test]
fn an_empty_composer_keeps_the_older_unmeasured_footer_policy() {
    let shown = "────────────────────────\n❯ \n────────────────────────\n";
    assert_eq!(screen::read(shown).footer, screen::Footer::Unknown);
    assert_eq!(composer_owner(shown), ComposerOwner::Empty);
}

#[test]
fn a_panicking_nested_chip_scope_cannot_replace_the_outer_attempts_witness() {
    let session = format!("outer-chip-{}", uuid::Uuid::new_v4());
    let inner_session = format!("inner-chip-{}", uuid::Uuid::new_v4());
    let frame = idle("ann", "7", "one\ntwo\nthree\nfour");
    let inner_frame = idle("ann", "7", "one\ntwo\nthree\nfour\nfive");
    let chip = capture("[Pasted text #14 +4 lines]");
    let inner_chip = capture("[Pasted text #15 +5 lines]");
    let report = ownership::with_chip_witness(&session, "1234abcd", &frame, || {
        let panic = std::panic::catch_unwind(|| {
            ownership::with_chip_witness(&inner_session, "5678abcd", &inner_frame, || {
                assert!(screen::owns_direct(
                    &inner_chip,
                    &inner_frame,
                    &screen::Drawn::Folded
                ));
                panic!("nested attempt stopped");
            });
        });
        assert!(panic.is_err());
        assert!(screen::owns_direct(&chip, &frame, &screen::Drawn::Folded));
        Report::new(
            Outcome::Unconfirmed(Unconfirmed::CursorChangedBeforeEnter),
            DraftState::Unknown,
        )
    });
    assert!(matches!(report.outcome, Outcome::Unconfirmed(_)));
    assert_eq!(
        composer_owner_in_session(&session, &chip),
        ComposerOwner::Unread
    );
    assert_eq!(
        composer_owner_in_session(&inner_session, &inner_chip),
        ComposerOwner::Person
    );

    ownership::with_chip_witness(&session, "90abcdef", &frame, || {
        assert!(screen::owns_direct(&chip, &frame, &screen::Drawn::Folded));
        Report::new(Outcome::Injected, DraftState::Unchanged)
    });
    assert_eq!(
        composer_owner_in_session(&session, &chip),
        ComposerOwner::Person,
        "a confirmed submission discards both old and current denial witnesses"
    );
}
