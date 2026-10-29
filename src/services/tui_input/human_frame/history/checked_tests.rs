use super::*;

#[test]
fn checked_observation_preserves_receipts_and_logical_session() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let native = tempfile::NamedTempFile::new().unwrap();
    let logical = "AgentDesk-claude-checked-owner";
    let frame = super::super::idle("ann", "720101", "next request");
    let prompts = [frame.clone()];
    register(
        ("claude", logical, "attempt", "720101"),
        native.path(),
        0,
        &frame,
    )
    .unwrap();
    assert!(matches!(
        observed_attempt_checked("claude", logical, &prompts, None),
        Ok(ObservedAttempt::Pending)
    ));
    assert_eq!(
        receipt(
            "attempt",
            native.path(),
            0,
            "user",
            Some("native-row"),
            &frame
        ),
        Some(true)
    );
    assert!(matches!(
        observed_attempt_checked("claude", logical, &prompts, Some("native-row")),
        Ok(ObservedAttempt::Known(id)) if id == "attempt"
    ));
    assert!(matches!(
        observed_attempt_checked("claude", logical, &prompts, Some("other-row")),
        Ok(ObservedAttempt::Pending)
    ));
    for (provider, session) in [("claude", "native-session-uuid"), ("codex", logical)] {
        assert!(matches!(
            observed_attempt_checked(provider, session, &prompts, Some("native-row")),
            Ok(ObservedAttempt::None)
        ));
    }
    assert!(matches!(
        observed_attempt_checked("claude", logical, &["human input".into()], None),
        Ok(ObservedAttempt::None)
    ));
}

#[test]
fn checked_observation_distinguishes_absence_from_history_failures() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let file = path().unwrap();
    let plain = ["unmarked native input".into()];
    let injected = ["[User: ann (ID: 720101)] injected input".into()];
    assert!(matches!(
        observed_attempt_checked("claude", "logical", &plain, None),
        Ok(ObservedAttempt::None)
    ));
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    for bytes in [
        b"{broken".as_slice(),
        br#"{"version":3,"live":[],"passive":[]}"#,
        br#"{"version":2,"live":[],"passive":[]}"#,
    ] {
        std::fs::write(&file, bytes).unwrap();
        for prompts in [&plain, &injected] {
            assert!(observed_attempt_checked("claude", "logical", prompts, None).is_err());
        }
        assert!(matches!(
            observed_attempt("claude", "logical", &plain, None),
            ObservedAttempt::None
        ));
        assert!(matches!(
            observed_attempt("claude", "logical", &injected, None),
            ObservedAttempt::Pending
        ));
    }
    let directory = tempfile::tempdir().unwrap();
    for unreadable in [
        Ok(directory.path().to_owned()),
        Err("history root unavailable".into()),
    ] {
        assert!(
            observed_at_checked(unreadable.clone(), "claude", "logical", &plain, None).is_err()
        );
        assert!(matches!(
            observed_at(unreadable.clone(), "claude", "logical", &plain, None),
            ObservedAttempt::None
        ));
        assert!(matches!(
            observed_at(unreadable, "claude", "logical", &injected, None),
            ObservedAttempt::Pending
        ));
    }
}
