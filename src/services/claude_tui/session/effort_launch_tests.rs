//! The generated settings and argv are the launch contract; no provider CLI starts.
use super::*;

fn with_launch(check: impl FnOnce(&mut ClaudeTuiLaunchConfig)) {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _context = crate::services::claude_compact_context::state_test_guard();
    let root = tempfile::tempdir().unwrap();
    let provider = root.path().join("launch-fixture.yaml");
    fs::write(
        &provider,
        serde_yaml::to_string(&serde_json::json!({
            "server": {}, "data": {"dir": root.path().join("data")}
        }))
        .unwrap(),
    )
    .unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _env = [
        set("AGENTDESK_CONFIG", &provider),
        set("AGENTDESK_ROOT_DIR", root.path()),
    ];
    check(&mut ClaudeTuiLaunchConfig {
        tmux_session_name: format!("fixture-claude-effort-{}", uuid::Uuid::new_v4()),
        working_dir: root.path().to_path_buf(),
        claude_bin: ClaudeBinary::from_tmux_wrapper_argv("/fixture/claude"),
        agentdesk_exe: PathBuf::from("/fixture/agentdesk"),
        hook_endpoint: "http://127.0.0.1:1".into(),
        session_id: "81090000-0000-4000-8000-000000000001".into(),
        system_prompt: None,
        model: Some("claude-opus-4-8".into()),
        resume: true,
    });
}

#[cfg(unix)]
#[test]
fn effort_resume_suppresses_native_continuation_before_owner_c() {
    use std::io::{BufRead, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    with_launch(|config| {
        let provider = config.working_dir.join("fake-provider");
        let transcript = config.working_dir.join("unfinished.jsonl");
        let requests = config.working_dir.join("requests");
        let unfinished = "{\"type\":\"user\",\"message\":\"unfinished task\"}\n";
        fs::write(&transcript, unfinished).unwrap();
        fs::write(
            &provider,
            r#"#!/bin/bash
set -eu
[[ " $* " == *" --resume "* ]]
[[ -s "$S3_TRANSCRIPT" ]]
if [[ "${CLAUDE_CODE_RESUME_INTERRUPTED_TURN:-}" == 1 ]]; then
    printf 'native continuation\n' >> "$S3_REQUESTS"
fi
printf 'READY\n'
IFS= read -r continuation
printf '%s\n' "$continuation" >> "$S3_REQUESTS"
"#,
        )
        .unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
        config.claude_bin = ClaudeBinary::from_tmux_wrapper_argv(provider.to_str().unwrap());
        let continuation =
            "effort 변경 처리를 위해 세션을 다시 시작했으니 중단된 작업을 이어서 진행하라";
        for effort in [Some("high"), Some("ultra"), None] {
            fs::write(&requests, "").unwrap();
            let files =
                prepare_claude_tui_launch_with_continuation_owner(config, None, effort, true)
                    .unwrap();
            let mut child = Command::new("/bin/bash")
                .arg(&files.launch_script_path)
                .env("CLAUDE_CODE_RESUME_INTERRUPTED_TURN", "1")
                .env("S3_TRANSCRIPT", &transcript)
                .env("S3_REQUESTS", &requests)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut ready = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            let before_c = fs::read_to_string(&requests).unwrap();
            let mut input = child.stdin.take().unwrap();
            writeln!(input, "{continuation}").unwrap();
            drop(input);
            let status = child.wait().unwrap();
            files.cleanup_best_effort();
            assert!(status.success());
            assert_eq!(ready, "READY\n");
            assert_eq!(fs::read_to_string(&transcript).unwrap(), unfinished);
            let native = "";
            assert_eq!(
                before_c, native,
                "C must own the first submit for {effort:?}"
            );
            assert_eq!(
                fs::read_to_string(&requests).unwrap(),
                format!("{native}{continuation}\n")
            );
        }
    });
}

fn read_launch(files: &ClaudeTuiSessionFiles) -> (serde_json::Value, String) {
    (
        serde_json::from_slice(&fs::read(&files.hook_settings_path).unwrap()).unwrap(),
        fs::read_to_string(&files.launch_script_path).unwrap(),
    )
}

#[test]
fn ultra_persists_and_normal_effort_turns_ultracode_off() {
    with_launch(|config| {
        for requested in ["ultra", "ultra", "low", "medium", "high", "xhigh", "max"] {
            let files =
                prepare_claude_tui_launch_with_settings(config, None, Some(requested)).unwrap();
            let (settings, script) = read_launch(&files);
            assert_eq!(
                settings["ultracode"],
                requested == "ultra",
                "{requested}: {settings}"
            );
            assert!(script.contains(&format!("'--resume' '{}'", config.session_id)));
            assert_eq!(script.matches("'--settings'").count(), 1);
            if requested == "ultra" {
                assert!(
                    !script.contains("'--effort'"),
                    "ultra uses settings: {script}"
                );
            } else {
                assert!(
                    script.contains(&format!("'--effort' '{requested}'")),
                    "{script}"
                );
            }
            assert!(
                script.contains("unset CLAUDE_CODE_EFFORT_LEVEL\n"),
                "{script}"
            );
            files.cleanup_best_effort();
        }
    });
}

#[test]
fn merged_settings_keep_hooks_fast() {
    with_launch(|config| {
        for (requested, fast) in [("ultra", true), ("high", false)] {
            let files =
                prepare_claude_tui_launch_with_settings(config, Some(fast), Some(requested))
                    .unwrap();
            let (settings, script) = read_launch(&files);
            assert_eq!(settings["ultracode"], requested == "ultra");
            assert_eq!(settings["fastMode"], fast);
            for event in ["SessionStart", "UserPromptSubmit", "Stop", "PostToolUse"] {
                assert!(settings["hooks"][event].is_array(), "{event}: {settings}");
            }
            assert!(settings["statusLine"].is_object());
            assert_eq!(script.matches("'--settings'").count(), 1);
            files.cleanup_best_effort();
        }
    });
}

#[test]
fn raw_ultracode_is_not_a_channel_ultra_alias() {
    with_launch(|config| {
        assert!(prepare_claude_tui_launch_with_settings(config, None, Some("ultracode")).is_err());
    });
}

#[test]
fn unset_channel_effort_preserves_the_cli_effort_setting() {
    with_launch(|config| {
        let files = prepare_claude_tui_launch_with_settings(config, None, None).unwrap();
        let (settings, script) = read_launch(&files);
        assert!(settings.get("ultracode").is_none());
        assert!(!script.contains("'--effort'"));
        assert!(!script.contains("unset CLAUDE_CODE_EFFORT_LEVEL\n"));
        files.cleanup_best_effort();
    });
}

#[cfg(unix)]
fn assert_interrupted_flag(resume: bool, owner_first_submit: bool, effort: Option<&str>) {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    with_launch(|config| {
        config.resume = resume;
        let provider = config.working_dir.join("flag-provider");
        fs::write(
            &provider,
            "#!/bin/bash\nprintf '%s\\n' \"${CLAUDE_CODE_RESUME_INTERRUPTED_TURN:-unset}\"\n",
        )
        .unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
        config.claude_bin = ClaudeBinary::from_tmux_wrapper_argv(provider.to_str().unwrap());
        let files = if owner_first_submit {
            prepare_claude_tui_launch_with_continuation_owner(config, None, effort, true)
        } else {
            prepare_claude_tui_launch_with_settings(config, None, effort)
        }
        .unwrap();
        let output = Command::new("/bin/bash")
            .arg(&files.launch_script_path)
            .env("CLAUDE_CODE_RESUME_INTERRUPTED_TURN", "1")
            .output()
            .unwrap();
        files.cleanup_best_effort();
        assert!(output.status.success(), "{:?}", output.stderr);
        let expected = if owner_first_submit && resume {
            "0\n"
        } else {
            "1\n"
        };
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    });
}

#[cfg(unix)]
#[test]
fn ordinary_resume_high_preserves_inherited_interrupted_flag() {
    assert_interrupted_flag(true, false, Some("high"));
}

#[cfg(unix)]
#[test]
fn ordinary_resume_unset_preserves_inherited_interrupted_flag() {
    assert_interrupted_flag(true, false, None);
}

#[cfg(unix)]
#[test]
fn effort_resume_high_suppresses_inherited_interrupted_flag() {
    assert_interrupted_flag(true, true, Some("high"));
}

#[cfg(unix)]
#[test]
fn effort_resume_unset_suppresses_inherited_interrupted_flag() {
    assert_interrupted_flag(true, true, None);
}

#[cfg(unix)]
#[test]
fn cold_start_high_preserves_inherited_interrupted_flag() {
    assert_interrupted_flag(false, false, Some("high"));
    assert_interrupted_flag(false, true, Some("high"));
}

#[cfg(unix)]
#[test]
fn cold_start_unset_preserves_inherited_interrupted_flag() {
    assert_interrupted_flag(false, false, None);
    assert_interrupted_flag(false, true, None);
}
