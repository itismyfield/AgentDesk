use super::{Projection, ShadowProvider, origin, project, record};
use crate::services::tui_input::human_frame::{self, history};

#[test]
fn projection_excludes_plain_injection_by_original_logical_session() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let transcript = tempfile::NamedTempFile::new().unwrap();
    let frame = human_frame::idle("ann", "733201", "continue");
    let row = record("injected-native", &frame);
    let owner = origin(&row);
    assert_ne!(owner.logical_session, owner.record.source.session_id);
    history::register(
        ("claude", &owner.logical_session, "attempt", "733201"),
        transcript.path(),
        0,
        &frame,
    )
    .unwrap();
    let wrapped = format!("<pasted_content id=\"a733\">\n{frame}\n</pasted_content id=\"a733\">");
    for received in [false, true] {
        if received {
            assert_eq!(
                history::receipt(
                    "attempt",
                    transcript.path(),
                    0,
                    "user",
                    Some("injected-native"),
                    &frame,
                ),
                Some(true)
            );
        }
        let observed = history::observed_attempt_checked(
            "claude",
            &owner.logical_session,
            std::slice::from_ref(&frame),
            Some("injected-native"),
        );
        assert!(matches!(
            (received, observed),
            (true, Ok(history::ObservedAttempt::Known(_)))
                | (false, Ok(history::ObservedAttempt::Pending))
        ));
        for text in [&frame, &wrapped] {
            let native = record("injected-native", text);
            let projected = project(&mut Projection::for_tests(), &native);
            // Pending ownership is held as evidence; only a receipt settles it as an injection.
            if received {
                assert!(
                    projected.unwrap().is_none(),
                    "a received injection creates no notice"
                );
            } else {
                assert_eq!(
                    projected.unwrap_err(),
                    "input notice injection ownership pending",
                    "an unreceived injection is held, never turned into a notice"
                );
            }
        }
    }
}

#[test]
fn projection_history_read_failure_blocks_plain_native_input() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let row = record("unmarked-native", "ordinary terminal input");
    assert!(
        project(&mut Projection::for_tests(), &row)
            .unwrap()
            .is_some(),
        "an absent history is a normal no-match"
    );
    let root = crate::config::runtime_root().unwrap();
    assert!(root.starts_with(std::env::temp_dir()));
    let file = root.join("runtime/injected_frames.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    for bytes in [
        b"{broken".as_slice(),
        br#"{"version":4,"live":[],"passive":[]}"#,
        br#"{"version":2,"live":7,"passive":[]}"#,
    ] {
        std::fs::write(&file, bytes).unwrap();
        assert!(
            project(&mut Projection::for_tests(), &row).is_err(),
            "unreadable evidence cannot promote a native row to a human notice"
        );
    }
    std::fs::write(file, br#"{"entries":[]}"#).unwrap();
    assert!(
        project(&mut Projection::for_tests(), &row)
            .unwrap()
            .is_some(),
        "a readable empty history restores a normal no-match"
    );
}

/// Typed input showing an attempt-tagged header nobody recorded, alone or quoted mid-text, is a
/// person's terminal input and gets its notice; the untagged relay header still gets none.
#[test]
fn an_unrecorded_tagged_header_in_typed_input_keeps_its_notice() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for typed in [
        "[User: bob (ID: 8) · deadbeef] hi",
        "이 형식을 검토해줘: [User: bob (ID: 8) · deadbeef] hi",
    ] {
        let row = record("typed-native", typed);
        assert!(
            project(&mut Projection::for_tests(), &row)
                .unwrap()
                .is_some(),
            "{typed}"
        );
    }
    let relay = record("relay-native", "[User: bob (ID: 8)] hi");
    assert!(
        project(&mut Projection::for_tests(), &relay)
            .unwrap()
            .is_none()
    );
}

/// While the history cannot be read, a recorded attempt-tagged frame is held by both readers: the
/// notice projection refuses yet keeps the raw row, and the observer opens no direct-input turn.
#[test]
fn an_unreadable_history_holds_a_recorded_tagged_frame_on_both_readers() {
    use crate::services::tui_prompt_dedupe::{PromptObservation, observe_codex_prompt_in_turn_at};
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let transcript = tempfile::NamedTempFile::new().unwrap();
    let frame = human_frame::plain_inject("d", "733201", "ann", "abcdef12", "continue");
    let owner = ("claude", "notice-test-pane", "abcdef12", "733201");
    history::register(owner, transcript.path(), 0, &frame).unwrap();
    let file = crate::config::runtime_root()
        .unwrap()
        .join("runtime/injected_frames.json");
    std::fs::write(&file, b"{broken").unwrap();
    let row = record("tagged-native", &frame);
    let mut projection = Projection::for_tests();
    let projected = projection.project_record(
        7332,
        ShadowProvider::Claude,
        &origin(&row).record.source,
        &row,
        Ok(origin(&row)),
    );
    assert!(
        projected.is_err(),
        "unreadable evidence cannot announce a recorded frame"
    );
    let kept = projection.take_evidence();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        (&kept[0].raw_record, kept[0].prompt.as_deref()),
        (&row.line, Some(frame.as_str()))
    );
    let prompts = std::slice::from_ref(&frame);
    let observed = history::observed_attempt("codex", "unreadable-pane", prompts, None);
    assert!(matches!(observed, history::ObservedAttempt::Pending));
    let seen = observe_codex_prompt_in_turn_at(
        "unreadable-pane",
        &frame,
        None,
        Some("t1"),
        chrono::Utc::now(),
    );
    assert_ne!(seen, PromptObservation::PublishedSshDirect);
}
