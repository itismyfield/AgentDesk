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
        ComposerOwner::Person
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
