use super::*;

#[test]
fn routine_model_requires_actual_argv_and_exact_execution_before_any_payload() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let wanted = Requirement {
        provider: "claude".into(),
        tmux: "adk-model-proof-fixture".into(),
        model: "opus".into(),
        resume: Some("same-native".into()),
    };
    let target =
        serde_json::json!({"host":"tmux","owner":wanted.tmux,"execution_nonce":"execution-b"});
    scope(wanted.clone(), || {
        assert!(before_payload(&target).is_err());
        assert!(
            prepared(
                "claude",
                &wanted.tmux,
                "fresh-substitute",
                false,
                "execution-b",
                &["--model".into(), "opus".into()]
            )
            .is_err()
        );
        assert!(
            prepared(
                "claude",
                &wanted.tmux,
                "same-native",
                true,
                "execution-b",
                &[
                    "--resume".into(),
                    "same-native".into(),
                    "--model".into(),
                    "sonnet".into()
                ]
            )
            .is_err()
        );
        prepared(
            "claude",
            &wanted.tmux,
            "same-native",
            true,
            "execution-b",
            &[
                "--resume".into(),
                "same-native".into(),
                "--model".into(),
                "opus".into(),
            ],
        )
        .unwrap();
        assert!(before_payload(&target).is_ok());
        let mut moved = target.clone();
        moved["execution_nonce"] = "execution-c".into();
        assert!(before_payload(&moved).is_err());
        moved = target.clone();
        moved["owner"] = "other-session".into();
        assert!(before_payload(&moved).is_err());
        assert!(!matches(
            "claude",
            &wanted.tmux,
            "execution-old",
            "same-native",
            "opus"
        ));
        assert!(!matches(
            "claude",
            &wanted.tmux,
            "execution-b",
            "same-native",
            "sonnet"
        ));
    });
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&wanted.tmux, "spawn_nonce"),
        "execution-b",
    )
    .unwrap();
    let keys = std::cell::Cell::new(0);
    let mut wrong = wanted.clone();
    wrong.model = "sonnet".into();
    scope(wrong, || {
        assert!(
            crate::services::claude_tui::owner_input::payload(|| {
                keys.set(keys.get() + 1);
                Ok(())
            })
            .is_err()
        );
    });
    assert_eq!(keys.get(), 0);
    scope(wanted.clone(), || {
        crate::services::claude_tui::owner_input::payload(|| {
            keys.set(keys.get() + 1);
            Ok(())
        })
        .unwrap();
    });
    assert_eq!(keys.get(), 1);
    assert!(current().is_none());
}

#[test]
fn routine_codex_model_never_substitutes_fresh_for_a_required_resume() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let wanted = Requirement {
        provider: "codex".into(),
        tmux: "adk-model-codex-fixture".into(),
        model: "codex-b".into(),
        resume: Some("native-codex".into()),
    };
    scope(wanted.clone(), || {
        assert!(
            prepared(
                "codex",
                &wanted.tmux,
                "native-codex",
                false,
                "nonce",
                &["-m".into(), "codex-b".into()]
            )
            .is_err()
        );
        prepared(
            "codex",
            &wanted.tmux,
            "native-codex",
            true,
            "nonce",
            &[
                "resume".into(),
                "native-codex".into(),
                "-m".into(),
                "codex-b".into(),
            ],
        )
        .unwrap();
        assert!(bind_session("codex", &wanted.tmux, "another-nonce", "native-codex").is_err());
        assert!(bind_session("codex", &wanted.tmux, "nonce", "another-native").is_err());
        assert!(matches(
            "codex",
            &wanted.tmux,
            "nonce",
            "native-codex",
            "codex-b"
        ));
    });
}
