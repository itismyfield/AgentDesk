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

#[cfg(unix)]
fn assert_codex_capacity_cold_chain(advance_rollout: bool) {
    use crate::config::TestEnvVarGuard as Guard;
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::codex_tui::warm_followup::{
        CodexWarmFallbackReason, write_codex_tui_launch_options_evidence,
    };
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context};
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use unicode_width::UnicodeWidthStr;

    struct SourceModeRestore(Option<CodexSourceMode>);
    impl Drop for SourceModeRestore {
        fn drop(&mut self) {
            SOURCE_MODE_TEST.with(|slot| slot.set(self.0));
        }
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _state = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (root, _env) = binding_context::tests::fixture_after_shared_test_env_lock();
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    let _mode = SourceModeRestore(
        SOURCE_MODE_TEST.with(|slot| slot.replace(Some(CodexSourceMode::Legacy))),
    );
    let _source = Guard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE",
        OsStr::new("legacy"),
    );
    let _hooks = Guard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
        OsStr::new("0"),
    );
    let _warm = Guard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_TUI_WARM_FOLLOWUP",
        OsStr::new("1"),
    );
    let _effort = Guard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_REASONING_EFFORT",
        OsStr::new(""),
    );
    let home = root.path().join("codex-home");
    let cwd = root.path().join("work");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let _home = Guard::set_path_after_shared_test_env_lock("CODEX_HOME", &home);
    let _fake_root =
        Guard::set_path_after_shared_test_env_lock("ADK_CODEX_COLD_CHAIN_ROOT", root.path());
    let tmux = if advance_rollout {
        "test-codex-capacity-cold-advanced"
    } else {
        "test-codex-capacity-cold-once"
    };
    let session_id = uuid::Uuid::new_v4().to_string();
    let rollout = home
        .join("sessions")
        .join(format!("rollout-{session_id}.jsonl"));
    let metadata = serde_json::json!({
        "type": "session_meta",
        "payload": {"id": session_id, "cwd": cwd, "source": "cli"},
    });
    std::fs::write(&rollout, format!("{metadata}\n")).unwrap();
    let pinned_len = std::fs::metadata(&rollout).unwrap().len();
    let _rollout =
        Guard::set_path_after_shared_test_env_lock("ADK_CODEX_COLD_CHAIN_ROLLOUT", &rollout);
    let script_path = crate::services::tmux_common::session_temp_path(tmux, "sh");
    std::fs::create_dir_all(std::path::Path::new(&script_path).parent().unwrap()).unwrap();
    let _script = Guard::set_path_after_shared_test_env_lock(
        "ADK_CODEX_COLD_CHAIN_SCRIPT",
        std::path::Path::new(&script_path),
    );
    let exit_reason = crate::services::tmux_common::session_temp_path(tmux, "exit_reason");
    let _reason = Guard::set_path_after_shared_test_env_lock(
        "ADK_CODEX_COLD_CHAIN_EXIT_REASON",
        std::path::Path::new(&exit_reason),
    );
    if advance_rollout {
        std::fs::write(root.path().join("advance-at-geometry"), "").unwrap();
    }
    let ready = format!(
        "╭{}╮\n│ ▌{}│\n╰{}╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send",
        "─".repeat(78),
        " ".repeat(76),
        "─".repeat(78),
    );
    let (marker, draft, _) =
        crate::services::codex_tui::input::prompt_readiness_from_ansi_pane(&ready);
    assert!(
        marker && !draft,
        "fixture must qualify for the actual warm submit"
    );
    std::fs::write(root.path().join("ready"), ready).unwrap();
    // Fourteen unfolded rows fit 80x24, while the full prompt exceeds the fold limit.
    let mut lines = vec!["x".repeat(73); 14];
    lines[0] = format!("-- '한글' {}", "x".repeat(62));
    let prompt = lines.join("\n");
    assert!(prompt.chars().count() > 1000);
    assert!(prompt.lines().all(|line| line.width() <= 76));
    assert!(crate::services::codex_tui::input::plan_prompt_submit(&prompt).is_ok());
    let options = CodexLaunchOptions::new(&prompt)
        .with_resume_session_id(Some(&session_id))
        .with_cwd(cwd.to_str());
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        tmux,
        RuntimeHandoffKind::CodexTui,
    )
    .unwrap();
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(
        tmux,
        &rollout,
        Some(&session_id),
    )
    .unwrap();
    write_codex_tui_launch_options_evidence(tmux, &options).unwrap();
    let records = [
        serde_json::json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"cold-capacity"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"capacity cold completed"}]}}),
        serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"cold-capacity","last_agent_message":"capacity cold completed"}}),
        serde_json::json!({"type":"event_msg","payload":{"type":"composer_ready","turn_id":"cold-capacity"}}),
    ].map(|record| format!("{record}\n")).concat();
    std::fs::write(root.path().join("cold.records"), records).unwrap();
    let write_program = |name: &str, body: &str| {
        let path = root.path().join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let codex = write_program(
        "codex",
        r#"#!/bin/bash
set -eu
root="$ADK_CODEX_COLD_CHAIN_ROOT"
if [ "${1-}" = --version ]; then
    printf 'codex-cli 0.157.1\n'
    exit 0
fi
printf 'cold-cli\n' >> "$root/cold.cli.calls"
printf '%s\0' "$@" > "$root/cold.argv"
cat "$root/cold.records" >> "$ADK_CODEX_COLD_CHAIN_ROLLOUT"
"#,
    );
    write_program(
        "tmux",
        r#"#!/bin/bash
set -eu
root="$ADK_CODEX_COLD_CHAIN_ROOT"
if [ "${1-}" = -u ]; then shift; fi
printf '%s\n' "$*" >> "$root/tmux.calls"
case "${1-}" in
has-session) exit 0 ;;
list-panes) printf '0\n' ;;
capture-pane) cat "$root/ready" ;;
display-message)
    case "${@: -1}" in
    '#{pane_width} #{pane_height}')
        printf 'geometry\n' >> "$root/geometry.calls"
        if [ -f "$root/advance-at-geometry" ] && [ ! -f "$root/advanced" ]; then
            printf '{"type":"event_msg","payload":{"type":"token_count"}}\n' >> "$ADK_CODEX_COLD_CHAIN_ROLLOUT"
            : > "$root/advanced"
        fi
        printf '80 24\n' ;;
    '#{pane_dead}') printf '0\n' ;;
    '#{pane_pid}') exit 0 ;;
    *) exit 0 ;;
    esac ;;
kill-session)
    cat "$ADK_CODEX_COLD_CHAIN_EXIT_REASON" > "$root/kill.reason" ;;
new-session)
    cp "$ADK_CODEX_COLD_CHAIN_SCRIPT" "$root/cold.script"
    /bin/bash -c "${@: -1}" ;;
load-buffer|paste-buffer|send-keys) exit 0 ;;
set-option|set-hook|set-environment|show-options|show-environment) exit 0 ;;
*) printf 'unexpected fake tmux command: %s\n' "$*" >&2; exit 64 ;;
esac
"#,
    );
    let _path = Guard::prepend_path_after_shared_test_env_lock(root.path());
    let _binary = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", &codex);
    let cancel = Arc::new(CancelToken::new());
    let finished = AtomicBool::new(false);
    let watchdog_fired = AtomicBool::new(false);
    let (sender, receiver) = std::sync::mpsc::channel();
    // A malformed completion must fail the assertions instead of hanging the suite.
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !finished.load(Ordering::Acquire) {
                watchdog_fired.store(true, Ordering::Release);
                cancel.cancelled.store(true, Ordering::Release);
            }
        });
        let result = execute_streaming_local_tui_tmux(
            &prompt,
            Some(&session_id),
            None,
            None,
            None,
            cwd.to_str().unwrap(),
            sender,
            Some(cancel.clone()),
            tmux,
            None,
            None,
            None,
            None,
            false,
            None,
            false,
        );
        finished.store(true, Ordering::Release);
        result
    });
    assert!(
        !watchdog_fired.load(Ordering::Acquire),
        "bounded fake completion timed out"
    );
    let calls = std::fs::read_to_string(root.path().join("tmux.calls")).unwrap();
    assert!(
        root.path().join("geometry.calls").exists(),
        "known geometry must be observed: result={result:?}; calls={calls}"
    );
    assert!(
        calls
            .lines()
            .all(|call| !["load-buffer", "paste-buffer", "send-keys"]
                .iter()
                .any(|kind| call.starts_with(kind))),
        "preflight mutated the pane: {calls}"
    );
    let cold_count = calls
        .lines()
        .filter(|call| call.starts_with("new-session "))
        .count();
    if advance_rollout {
        let error = result.expect_err("rollout advancement must forbid cold replay");
        assert!(
            error.contains("failed before Enter but rollout advanced; refusing replay"),
            "{error}"
        );
        assert!(root.path().join("advanced").exists());
        assert!(std::fs::metadata(&rollout).unwrap().len() > pinned_len);
        assert_eq!(cold_count, 0, "{calls}");
        assert!(!root.path().join("cold.argv").exists());
        assert!(
            !calls.lines().any(|call| call.starts_with("kill-session ")),
            "{calls}"
        );
    } else {
        assert_eq!(result, Ok(()));
        assert_eq!(cold_count, 1, "{calls}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("cold.cli.calls")).unwrap(),
            "cold-cli\n"
        );
        let argv = std::fs::read(root.path().join("cold.argv")).unwrap();
        let args: Vec<&[u8]> = argv
            .strip_suffix(&[0])
            .unwrap()
            .split(|byte| *byte == 0)
            .collect();
        assert_eq!(args.last().copied(), Some(prompt.as_bytes()));
        assert_eq!(
            args.iter().filter(|arg| **arg == prompt.as_bytes()).count(),
            1
        );
        let reason = std::fs::read_to_string(root.path().join("kill.reason")).unwrap();
        assert!(
            reason.contains(CodexWarmFallbackReason::SubmitFailed.reason_text()),
            "{reason}"
        );
        let messages: Vec<_> = receiver.try_iter().collect();
        let completed: Vec<_> = messages
            .iter()
            .filter_map(|message| match message {
                StreamMessage::CodexTuiTerminalDone {
                    result,
                    rollout_path,
                    tmux_session_name,
                    source_start,
                    complete_record_end,
                    kind,
                    ..
                } => Some((
                    result,
                    rollout_path,
                    tmux_session_name,
                    source_start,
                    complete_record_end,
                    kind,
                )),
                _ => None,
            })
            .collect();
        assert_eq!(completed.len(), 1, "real cold terminal: {messages:?}");
        let (result, path, session, start, end, kind) = completed[0];
        assert_eq!(result, "capacity cold completed");
        assert_eq!(std::path::Path::new(path), rollout.as_path());
        assert_eq!(session, tmux);
        assert_eq!(*start, pinned_len);
        assert!(*end > *start && *end <= std::fs::metadata(&rollout).unwrap().len());
        assert!(kind.is_completed());
    }
}

#[cfg(unix)]
#[test]
fn codex_capacity_preflight_replays_oversize_multiline_once_with_identical_bytes() {
    assert_codex_capacity_cold_chain(false);
}

#[cfg(unix)]
#[test]
fn codex_capacity_preflight_holds_when_rollout_advances_before_cold_launch() {
    assert_codex_capacity_cold_chain(true);
}
