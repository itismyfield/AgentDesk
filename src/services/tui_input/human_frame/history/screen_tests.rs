use super::*;

#[test]
fn frame_history_follows_each_test_runtime_root() {
    let transcript = tempfile::NamedTempFile::new().unwrap();
    let frame = super::super::idle("isolation", "7196", "runtime isolation");
    {
        let _runtime = crate::config::TestRuntimeRootGuard::new();
        register(
            ("claude", "isolated-screen", "isolation-row", "7196"),
            transcript.path(),
            0,
            &frame,
        )
        .unwrap();
        assert!(known(&frame));
        assert!(screen_pending("isolated-screen", &frame));
    }
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    assert!(
        !known(&frame),
        "a prior test's durable history must not enter this runtime"
    );
    assert!(!screen_pending("isolated-screen", &frame));
}

#[test]
fn pending_screen_matches_folded_body_and_ignores_other_providers() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let transcript = tempfile::NamedTempFile::new().unwrap();
    let frame = super::super::idle("ann", "7", "please inspect\nthis body");
    register(
        ("claude", "folded-screen", "folded-row", "7"),
        transcript.path(),
        0,
        &frame,
    )
    .unwrap();
    assert!(screen_pending(
        "folded-screen",
        "[User: ann\n (ID: 7)] please in\nspect this body"
    ));
    assert!(!screen_pending(
        "folded-screen",
        "[User: ann (ID: 7)] another body"
    ));
    assert!(!screen_pending("another-screen", &frame));
    register(
        ("codex", "codex-screen", "codex-screen-row", "7"),
        transcript.path(),
        0,
        &frame,
    )
    .unwrap();
    assert!(!screen_pending("codex-screen", &frame));
}

#[test]
fn confirmed_author_history_preserves_all_ten_idle_recovery_regressions() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let transcript = tempfile::NamedTempFile::new().unwrap();
    for (name, author) in [
        ("0hbujang", "343742347365974026"),
        ("ann", "7"),
        ("명령봇", "1"),
    ] {
        let frame = super::super::idle(name, author, "confirmed in another session");
        register(
            ("claude", "finished-other-session", author, author),
            transcript.path(),
            0,
            &frame,
        )
        .unwrap();
        assert_eq!(
            receipt(
                author,
                transcript.path(),
                0,
                "enqueue",
                Some(author),
                &frame
            ),
            Some(true)
        );
    }
    let ledger = path().unwrap();
    let before = std::fs::read(&ledger).unwrap();
    let names = [
        "services::claude::claude_tui_session_resolution_tests::stranded_followup_user_draft_below_finished_block_fires_recovery",
        "services::claude::claude_tui_session_resolution_tests::stranded_followup_user_draft_below_zero_tool_block_fires_recovery",
        "services::claude_tui::hosting::followup_support::claude_tui_prompt_buffer_tests::ignores_submitted_discord_history_prompt",
        "services::claude_tui::hosting::host_draft::tests::tf2_legacy_tmux_clear_then_submits_once",
        "services::claude_tui::hosting::host_draft::tests::tf2_legacy_tmux_recreates_only_after_retiring_the_session",
        "services::claude_tui::hosting::host_draft::tests::tf2_swap_stop_or_lingering_draft_never_recreates",
        "services::claude_tui::input::tests::claude_prompt_draft_backspace_budget_ignores_submitted_discord_prompt_history",
        "services::tmux_common::sentinel_tests::claude_prompt_draft_detector_ignores_visibly_running_user_turn_with_spinner",
        "services::tmux_common::sentinel_tests::claude_prompt_draft_detector_recovers_stranded_followup_below_finished_block",
        "services::tmux_common::sentinel_tests::claude_prompt_draft_detector_recovers_stranded_followup_below_zero_tool_block",
    ];
    let root = crate::config::runtime_root().unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(names)
        .args(["--exact", "--test-threads=1", "--nocapture"])
        .env("ADK_TEST_FRAME_LEDGER_PATH", &ledger)
        .env("AGENTDESK_ROOT_DIR", &root)
        .env("AGENTDESK_CONFIG", root.join("config/agentdesk.yaml"))
        .output()
        .unwrap();
    let output = String::from_utf8_lossy(&child.stdout);
    assert!(
        child.status.success(),
        "nonempty-history idle regressions failed: {output}{}",
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(
        output.contains("10 passed; 0 failed; 0 ignored"),
        "all ten original regressions must execute: {output}"
    );
    println!("confirmed-history regressions:\n{output}");
    assert_eq!(
        std::fs::read(&ledger).unwrap(),
        before,
        "read-only idle polling must preserve confirmed history"
    );
}
