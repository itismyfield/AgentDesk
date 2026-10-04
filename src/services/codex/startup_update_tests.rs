use super::*;

#[test]
fn startup_update_check_is_disabled_for_fresh_resume_and_readonly_tui() {
    for resume in [None, Some("session-123")] {
        for readonly in [false, true] {
            let args = build_codex_tui_args(
                &CodexLaunchOptions::new("prompt")
                    .with_resume_session_id(resume)
                    .with_readonly_mode(readonly),
            );
            let overrides: Vec<_> = args
                .windows(2)
                .filter(|pair| pair[0] == "-c")
                .map(|pair| pair[1].as_str())
                .collect();
            assert!(overrides.contains(&"check_for_update_on_startup=false"));
            let script = render_codex_tui_tmux_script("", "/opt/bin/codex", &args);
            assert!(script.contains("'-c' 'check_for_update_on_startup=false'"));
        }
    }
}

#[test]
fn codex_tui_args_snapshot_preserves_common_launch_options() {
    let args = build_codex_tui_args(
        &CodexLaunchOptions::new("prompt that starts --flag")
            .with_resume_session_id(Some("session-123"))
            .with_model(Some("gpt-5-codex"))
            .with_reasoning_effort(Some("xhigh"))
            .with_compact_token_limit(Some(120_000))
            .with_readonly_mode(true)
            .with_fast_mode_enabled(Some(false))
            .with_goals_enabled(Some(true))
            .with_cwd(Some("/work/repo"))
            .with_add_dirs(&["/work/shared", "  /work/second  "]),
    );

    assert_eq!(
        args,
        vec![
            "resume",
            "-c",
            r#"model_reasoning_effort="xhigh""#,
            "-c",
            "model_auto_compact_token_limit=120000",
            "-m",
            "gpt-5-codex",
            "--disable",
            "fast_mode",
            "--enable",
            "goals",
            "-C",
            "/work/repo",
            "--add-dir",
            "/work/shared",
            "--add-dir",
            "/work/second",
            "-c",
            "check_for_update_on_startup=false",
            "--sandbox",
            "read-only",
            "session-123",
            "--",
            "prompt that starts --flag",
        ]
    );
}

#[test]
fn codex_tui_script_snapshot_preserves_env_and_command_shape() {
    let env_lines = build_tmux_launch_env_lines(
        Some("/opt/codex/bin:/usr/bin"),
        Some(42),
        Some(ProviderKind::Codex),
    );
    let args = build_codex_tui_args(
        &CodexLaunchOptions::new("fresh prompt")
            .with_model(Some("gpt-5-codex"))
            .with_reasoning_effort(Some("medium"))
            .with_compact_token_limit(Some(64_000))
            .with_readonly_mode(false)
            .with_cwd(Some("/work/repo")),
    );
    let script = render_codex_tui_tmux_script(&env_lines, "/opt/bin/codex", &args);

    assert!(script.contains("unset CLAUDECODE\n"));
    assert!(script.contains("export PATH='/opt/codex/bin:/usr/bin'\n"));
    assert!(script.contains(&format!("export {RESTART_REPORT_CHANNEL_ENV}=42\n")));
    assert!(script.contains(&format!("export {RESTART_REPORT_PROVIDER_ENV}=codex\n")));
    if std::env::var("AGENTDESK_ROOT_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .is_some()
    {
        assert!(script.contains("export AGENTDESK_ROOT_DIR="));
    }
    assert!(script.ends_with("exec '/opt/bin/codex' '-c' 'model_reasoning_effort=\"medium\"' '-c' 'model_auto_compact_token_limit=64000' '-m' 'gpt-5-codex' '-C' '/work/repo' '-c' 'check_for_update_on_startup=false' '--dangerously-bypass-approvals-and-sandbox' '--' 'fresh prompt'\n"));
}
