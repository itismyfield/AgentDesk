use super::{Projection, origin, project, record};
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
        br#"{"version":3,"live":[],"passive":[]}"#,
        br#"{"version":2,"live":[],"passive":[]}"#,
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
