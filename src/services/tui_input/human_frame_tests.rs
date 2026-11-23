use super::human_frame::{
    history, idle, opens_inject, parse, plain_inject, preserved_decision_body,
};

#[test]
fn busy_header_bytes_equal_idle_plus_attempt_tag_for_one_line_and_multiline() {
    let frame = crate::services::claude_tui::busy_inject::frame;
    for (body, expected) in [
        ("one line", "[User: ann (ID: 200) · abcdef12] one line"),
        ("one\ntwo", "[User: ann (ID: 200) · abcdef12]\none\ntwo"),
        (
            "one\r\ntwo\rthree",
            "[User: ann (ID: 200) · abcdef12]\none\ntwo\nthree",
        ),
    ] {
        assert_eq!(
            frame("discord:7:9", "200", "ann", "abcdef12", body).as_bytes(),
            expected.as_bytes()
        );
    }
}

/// The byte-keeping planners take only a frame an approved opinion built. A source name, the
/// wrong event kind or a changed body builds none, so such input keeps the normalizing plan.
#[test]
fn decision_preserved_plan_needs_an_event_that_approved_the_exact_opinion() {
    use crate::db::decisions::{DecisionProvenance, EventKind};
    use crate::services::{claude_tui, codex_tui};
    let raw = "첫 줄\r\n둘째 줄\r끝";
    let opinion = DecisionProvenance::for_tests(EventKind::Opinion, 200, raw);
    let frame = preserved_decision_body(&opinion, "ann", raw).expect("approved opinion");
    assert_eq!(frame.as_str(), format!("[User: ann (ID: 200)]\n{raw}"));
    let claude = claude_tui::input::plan_preserved_submit(&frame).unwrap();
    let kept = claude_tui::input::TuiInputAction::PasteBuffer(frame.as_str().to_string());
    assert_eq!(claude[0], kept);
    let codex = codex_tui::input::owned_preserved_plan(&frame).unwrap();
    let kept = codex_tui::input::TuiInputAction::PasteBuffer(frame.as_str().to_string());
    assert_eq!(codex[0], kept);

    // No approval for these bytes: no frame, whatever the input calls itself.
    let selection = DecisionProvenance::for_tests(EventKind::Selection, 200, raw);
    assert!(preserved_decision_body(&selection, "ann", raw).is_none());
    assert!(preserved_decision_body(&opinion, "ann", "첫 줄\n둘째 줄\n끝").is_none());
    // The only path left to a self-declared source=decision input rewrites its line endings.
    let declared = plain_inject("decision", "200", "ann", "decision:forged", raw);
    assert_eq!(declared, "[User: ann (ID: 200)]\n첫 줄\n둘째 줄\n끝");
    for plan in [
        format!(
            "{:?}",
            claude_tui::input::plan_prompt_submit(&format!("x\n{raw}"))
        ),
        format!(
            "{:?}",
            codex_tui::input::owned_prompt_plan(&format!("x\n{raw}"))
        ),
    ] {
        assert!(!plan.contains("\\r"), "{plan}");
    }
}

/// Two attempts of one text differ by their tag; tagged and untagged frames the ledger holds both
/// read as injected, and a header nobody recorded keeps the class it has without a tag.
#[test]
fn a_tagged_busy_frame_reads_as_its_untagged_one() {
    use crate::services::tui_prompt_dedupe::is_discord_relayed_user_prompt as relayed;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let [first, second] =
        ["abcdef12", "0123abcd"].map(|n| plain_inject("d", "7", "ann", n, "same"));
    assert_ne!(first, second);
    let transcript = crate::config::runtime_root().unwrap().join("t.jsonl");
    std::fs::write(&transcript, "").unwrap();
    for (nonce, frame) in [("abcdef12", first), ("1e1e1e1e", idle("ann", "7", "same"))] {
        history::register(("claude", "s", nonce, "7"), &transcript, 0, &frame).unwrap();
        let (header, body) = parse(&frame).expect("a recorded frame reads as injected");
        let read = (header.name, header.author, body);
        assert_eq!(read, ("ann", "7", " same"), "{frame}");
        assert!(opens_inject(&frame) && !relayed(&frame), "{frame}");
    }
    // Only the untagged header is the idle relay's; a tag nobody recorded is no relay evidence.
    for (typed, new_turn) in [("", true), (" · deadbeef", false), (" · beef", false)] {
        let typed = format!("[User: bob (ID: 8){typed}] hi");
        assert!(parse(&typed).is_none() && !opens_inject(&typed), "{typed}");
        assert_eq!(relayed(&typed), new_turn, "{typed}");
    }
    let quoted = "이 형식을 검토해줘: [User: bob (ID: 8) · deadbeef] hi";
    assert!(!opens_inject(quoted) && !relayed(quoted), "{quoted}");
}
