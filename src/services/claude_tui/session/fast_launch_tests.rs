//! The fast setting a launch hands its Claude TUI, read from the files the launch writes.
use super::*;

// The channel's fast setting rides in the one settings file beside the hooks and status line: on,
// off, or left to the CLI when unset; the resumed session keeps its id.
#[test]
fn claude_tui_launch_carries_fast_beside_hooks_and_resume() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _context = crate::services::claude_compact_context::state_test_guard();
    let root = tempfile::tempdir().unwrap();
    let provider = super::auto_compact_launch_tests::write_provider_setting(root.path(), None);
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _env = [
        set("AGENTDESK_CONFIG", &provider),
        set("AGENTDESK_ROOT_DIR", root.path()),
    ];
    let session = "64860000-0000-4000-8000-000000000003";
    for (fast, expected) in [
        (Some(true), Some(true)),
        (Some(false), Some(false)),
        (None, None),
    ] {
        let config = ClaudeTuiLaunchConfig {
            tmux_session_name: format!("AgentDesk-claude-6486-{}", uuid::Uuid::new_v4()),
            working_dir: root.path().to_path_buf(),
            claude_bin: ClaudeBinary::from_tmux_wrapper_argv("/fixture/claude"),
            agentdesk_exe: PathBuf::from("/fixture/agentdesk"),
            hook_endpoint: "http://127.0.0.1:1".to_string(),
            session_id: session.to_string(),
            system_prompt: None,
            model: None,
            resume: true,
        };
        let files = prepare_claude_tui_launch_with_fast(&config, fast).unwrap();
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(&files.hook_settings_path).unwrap()).unwrap();
        assert_eq!(
            settings["fastMode"].as_bool(),
            expected,
            "{fast:?}: {settings}"
        );
        assert_eq!(
            settings.get("fastMode").is_some(),
            fast.is_some(),
            "{fast:?}"
        );
        assert!(settings["hooks"].is_object(), "{fast:?}: {settings}");
        assert!(settings["statusLine"].is_object(), "{fast:?}: {settings}");
        let script = fs::read_to_string(&files.launch_script_path).unwrap();
        assert!(
            script.contains(&format!("'--resume' '{session}'")),
            "{script}"
        );
        let _ = fs::remove_file(&files.hook_settings_path);
        let _ = fs::remove_file(&files.launch_script_path);
    }
}
