use super::*;

#[test]
fn herdr_fresh_argv_rejects_resume_without_rejecting_option_values() {
    let args = |parts: &[&str]| {
        parts
            .iter()
            .map(|part| (*part).to_owned())
            .collect::<Vec<_>>()
    };
    for (mode, resume, argv) in [
        ("resume", None, args(&[])),
        ("fresh", Some("saved-session"), args(&[])),
        ("fresh", None, args(&["resume", "saved-session"])),
        (
            "fresh",
            None,
            args(&["--no-daemon", "-m", "gpt", "resume", "saved-session"]),
        ),
    ] {
        assert!(
            validate_herdr_fresh_argv(mode, resume, &argv).is_err(),
            "fresh_argv_no_resume_contract: {mode:?} {resume:?} {argv:?}"
        );
    }
    for argv in [
        args(&[]),
        args(&[
            "--no-daemon",
            "-m",
            "resume",
            "-C",
            "resume",
            "--add-dir",
            "resume",
            "--enable",
            "resume",
        ]),
    ] {
        assert_eq!(
            validate_herdr_fresh_argv("fresh", None, &argv),
            Ok(()),
            "fresh_argv_values_not_commands_contract"
        );
    }
    assert!(
        validate_herdr_fresh_argv(
            "fresh",
            None,
            &args(&["--unknown", "value", "resume", "saved-session"])
        )
        .is_err(),
        "fresh_unknown_option_cannot_hide_resume_contract"
    );
    let options = CodexLaunchOptions {
        model: Some("resume".into()),
        cwd: Some("resume".into()),
        ..Default::default()
    };
    assert_eq!(
        validate_herdr_fresh_argv("fresh", None, &codex_tui_option_args(&options)),
        Ok(())
    );
}

#[test]
fn codex_herdr_prepare_rejects_resume_before_launch_effects() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let mut prepared =
        PreparedIncarnation::prepare_at("codex", "fresh-argv", Some(1), None, false, None).unwrap();
    let launchable = CodexHerdrLaunchable {
        codex_bin: "/must-not-execute".into(),
        resolution: crate::services::platform::BinaryResolution {
            resolved_path: Some("/must-not-execute".into()),
            requested_binary: "codex".into(),
            canonical_path: None,
            source: None,
            attempts: vec![],
            failure_kind: None,
            exec_path: None,
        },
        policy: "verified",
    };
    let overlay = crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
        ProviderKind::Codex,
    );
    for (mode, resume) in [("resume", None), ("fresh", Some("saved-session"))] {
        prepared.context.launch_mode = mode.into();
        let options = CodexLaunchOptions {
            resume_session_id: resume.map(str::to_owned),
            ..Default::default()
        };
        let result =
            prepare_codex_herdr_launch(&prepared, &launchable, &options, &overlay, root.path());
        assert!(
            matches!(result, Err(error) if error.contains("fresh mode")),
            "actual_codex_prepare_refuses_resume_contract"
        );
        let script = crate::services::tmux_common::session_temp_path("fresh-argv", "sh");
        assert!(
            !std::path::Path::new(&script).exists(),
            "resume refusal precedes script publication"
        );
    }
}
