//! Tui session launch.

use super::*;
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;

/// Prepare durable launch evidence before creating the hosted tmux session.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_and_create_claude_tui_session(
    tmux_session_name: &str,
    working_dir: &str,
    working_dir_path: &std::path::Path,
    resolved_session_id: &str,
    system_prompt: Option<&str>,
    model_override: Option<&str>,
    hook_endpoint: String,
    resume: bool,
    auth_env_lines: &str,
    channel_id: Option<u64>,
) -> Result<(String, PreparedIncarnation), String> {
    use crate::services::herdr_launch::{HERDR_NOT_ADMITTED, herdr_admitted_for_claude_launch};
    // The host is chosen before any launch I/O; only tmux is admitted.
    if herdr_admitted_for_claude_launch(channel_id) {
        return Err(HERDR_NOT_ADMITTED.to_string());
    }
    crate::services::tmux_common::cleanup_session_temp_files(tmux_session_name);
    write_tmux_owner_marker(tmux_session_name)?;
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        tmux_session_name,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )?;
    let owner_path = tmux_owner_path(tmux_session_name);
    let mut prepared_session_files = None;
    let launch_result = (|| -> Result<(_, PreparedIncarnation), String> {
        let prepared = PreparedIncarnation::prepare(
            "claude",
            tmux_session_name,
            channel_id,
            Some(resolved_session_id),
            resume,
        )?;
        crate::services::tmux_common::host_marker::record_tmux_host_marker(tmux_session_name);
        let exe =
            std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;
        let (claude_bin, _resolution) = resolve_claude_binary()?;
        let launch_config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
            tmux_session_name: tmux_session_name.to_string(),
            working_dir: working_dir_path.to_path_buf(),
            claude_bin,
            agentdesk_exe: exe,
            hook_endpoint,
            session_id: resolved_session_id.to_string(),
            system_prompt: system_prompt.map(str::to_string),
            model: model_override.map(str::to_string),
            resume,
        };
        let session_files =
            crate::services::claude_tui::session::prepare_claude_tui_launch(&launch_config)?;
        let launch_script_path = session_files.launch_script_path.clone();
        prepared_session_files = Some(session_files);
        let script = std::fs::read_to_string(&launch_script_path)
            .map_err(|error| format!("read Claude TUI launch script: {error}"))?;
        let script = script.replacen(
            "#!/bin/bash\n",
            &format!("#!/bin/bash\n{}{auth_env_lines}", prepared.env_lines()),
            1,
        );
        std::fs::write(&launch_script_path, script)
            .map_err(|error| format!("update Claude TUI launch script: {error}"))?;
        let result = crate::services::platform::tmux::create_session(
            tmux_session_name,
            Some(working_dir),
            &format!(
                "bash {}",
                shell_escape(&launch_script_path.display().to_string())
            ),
        )?;
        Ok((result, prepared))
    })();
    let (tmux_result, incarnation) = match launch_result {
        Ok(result) => result,
        Err(error) => {
            if let Some(files) = prepared_session_files.as_ref() {
                files.cleanup_best_effort();
            }
            let _ = std::fs::remove_file(&owner_path);
            return Err(error);
        }
    };
    if !tmux_result.status.success() {
        let stderr = String::from_utf8_lossy(&tmux_result.stderr);
        if let Some(files) = prepared_session_files.as_ref() {
            files.cleanup_best_effort();
        }
        let _ = std::fs::remove_file(&owner_path);
        return Err(format!("tmux error: {}", stderr));
    }
    Ok((owner_path, incarnation))
}

/// Herdr pane command for one prepared incarnation: the same launch script as tmux, with
/// the Herdr pane variables removed right before the provider exec.
#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn prepare_claude_herdr_launch(
    prepared: &PreparedIncarnation,
    working_dir_path: &std::path::Path,
    resolved_session_id: &str,
    system_prompt: Option<&str>,
    model_override: Option<&str>,
    hook_endpoint: String,
    auth_env_lines: &str,
) -> Result<crate::services::herdr_launch::HerdrLaunchCommand, String> {
    use crate::services::herdr_launch::{HerdrLaunchCommand, unset_herdr_env_before_exec};
    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;
    let (claude_bin, _resolution) = resolve_claude_binary()?;
    let launch_config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
        tmux_session_name: prepared.context.tmux_session.clone(),
        working_dir: working_dir_path.to_path_buf(),
        claude_bin,
        agentdesk_exe: exe,
        hook_endpoint,
        session_id: resolved_session_id.to_string(),
        system_prompt: system_prompt.map(str::to_string),
        model: model_override.map(str::to_string),
        resume: prepared.context.launch_mode == "resume",
    };
    let files = crate::services::claude_tui::session::prepare_claude_tui_launch(&launch_config)?;
    let path = &files.launch_script_path;
    let written = std::fs::read_to_string(path)
        .map_err(|error| format!("read Claude TUI launch script: {error}"))
        .and_then(|script| unset_herdr_env_before_exec(&script))
        .and_then(|script| {
            let exports = format!("#!/bin/bash\n{}{auth_env_lines}", prepared.env_lines());
            std::fs::write(path, script.replacen("#!/bin/bash\n", &exports, 1))
                .map_err(|error| format!("update Claude TUI launch script: {error}"))
        });
    if let Err(error) = written {
        files.cleanup_best_effort();
        return Err(error);
    }
    Ok(HerdrLaunchCommand {
        cwd: working_dir_path.to_path_buf(),
        command: format!("bash {}", shell_escape(&path.display().to_string())),
    })
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn binding_context_t7_claude_launch_fails_before_tmux() {
        use super::prepare_and_create_claude_tui_session as launch;
        let dir = "/private/tmp";
        let cwd = std::path::Path::new(dir);
        let id = "11111111-1111-4111-8111-111111111111";
        crate::services::tui_prompt_dedupe::binding_context::tests::launch_failures(
            |t| launch(t, dir, cwd, id, None, None, "".into(), false, "", None).map(|_| ()),
            crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        );
    }
}

#[cfg(test)]
mod host_marker_tests {
    #[cfg(unix)]
    #[test]
    fn claude_launch_marks_its_tmux_host_where_session_cleanup_looks_and_a_failed_mark_still_launches()
     {
        use super::prepare_and_create_claude_tui_session as launch;
        use crate::config::TestEnvVarGuard as Guard;
        use crate::services::discord::session_identity::tmux_name_from_session_key;
        use crate::services::session_host::HostKind;
        use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
        use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::tests};
        use std::os::unix::fs::PermissionsExt;
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, _env) = tests::fixture_after_shared_test_env_lock();
        // Prepend the fake tmux: concurrent tests must still find system binaries.
        let stub = "#!/bin/bash\nprintf '%s\\n' \"$*\" >> \"$AGENTDESK_ROOT_DIR/tmux.calls\"\n";
        let fake_tmux = root.path().join("tmux");
        std::fs::write(&fake_tmux, stub).unwrap();
        std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = format!(
            "{}:{}",
            root.path().display(),
            std::env::var("PATH").unwrap()
        );
        let _path = Guard::set_value_after_shared_test_env_lock("PATH", path.as_ref());
        let claude = root.path().join("claude");
        std::fs::write(&claude, "#!/bin/bash\necho '2.1.0 (Claude Code)'\n").unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _bin = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CLAUDE_PATH", &claude);
        let dir = root.path().to_str().unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        let calls = root.path().join("tmux.calls");

        let tmux = "AgentDesk-claude-host-marker-launch";
        let session_key = format!("claude/token-hash/mac-mini:{tmux}");
        let witness_name = tmux_name_from_session_key(&session_key).unwrap();
        assert_eq!(read_host_kind_marker(&witness_name), HostKindMarker::Absent);
        launch(
            tmux,
            dir,
            root.path(),
            id,
            None,
            None,
            "".into(),
            false,
            "",
            Some(42),
        )
        .unwrap();
        assert!(std::fs::read_to_string(&calls).unwrap().contains(tmux));
        assert_eq!(
            read_host_kind_marker(&witness_name),
            HostKindMarker::Known(HostKind::Tmux),
            "cleanup reads the marker by the session key's tmux name"
        );

        let blocked = "AgentDesk-claude-host-marker-blocked";
        let marker = crate::services::tmux_common::session_temp_path(blocked, "host_kind");
        std::fs::create_dir(&marker).unwrap();
        launch(
            blocked,
            dir,
            root.path(),
            id,
            None,
            None,
            "".into(),
            false,
            "",
            Some(43),
        )
        .expect("a marker write failure must not block the launch");
        assert!(std::fs::read_to_string(&calls).unwrap().contains(blocked));
        assert!(matches!(
            read_host_kind_marker(blocked),
            HostKindMarker::ReadFailed(_)
        ));
    }
}

#[cfg(test)]
mod herdr_off_tests {
    #[cfg(unix)]
    #[test]
    fn claude_launch_entry_stays_on_tmux_and_starts_no_herdr_preparation() {
        use super::prepare_and_create_claude_tui_session as launch;
        use crate::config::TestEnvVarGuard as Guard;
        use crate::services::session_host::HostKind;
        use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
        use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::tests};
        use std::os::unix::fs::PermissionsExt;
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, _env) = tests::fixture_after_shared_test_env_lock();
        let executable = |name: &str, body: &str| {
            let path = root.path().join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        };
        executable(
            "tmux",
            "#!/bin/bash\nprintf '%s\\n' \"$*\" >> \"$AGENTDESK_ROOT_DIR/tmux.calls\"\n",
        );
        let claude = executable("claude", "#!/bin/bash\necho '2.1.0 (Claude Code)'\n");
        let path = format!(
            "{}:{}",
            root.path().display(),
            std::env::var("PATH").unwrap()
        );
        let _path = Guard::set_value_after_shared_test_env_lock("PATH", path.as_ref());
        let _bin = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CLAUDE_PATH", &claude);
        let tmux = "AgentDesk-claude-herdr-off";
        let id = "11111111-1111-4111-8111-111111111111";
        let dir = root.path().to_str().unwrap();

        let launched = launch(
            tmux,
            dir,
            root.path(),
            id,
            None,
            None,
            "".into(),
            false,
            "",
            Some(44),
        );

        launched.expect("the tmux launch runs as before");
        let calls = std::fs::read_to_string(root.path().join("tmux.calls")).unwrap();
        assert!(
            calls.contains(&format!("new-session -d -s {tmux}")),
            "{calls}"
        );
        assert_eq!(
            read_host_kind_marker(tmux),
            HostKindMarker::Known(HostKind::Tmux)
        );
        assert_eq!(
            crate::services::herdr_launch::admissions_on_this_thread(),
            0
        );
    }

    // The launch entry reads O readiness only for a selected channel, keeps an unready one on
    // tmux, and leaves tmux only for a channel whose O writer already holds a seeded store.
    #[cfg(unix)]
    #[test]
    fn claude_launch_takes_herdr_only_for_a_selected_channel_with_a_ready_o_store() {
        use super::prepare_and_create_claude_tui_session as launch;
        use crate::config::TestEnvVarGuard as Guard;
        use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
        use crate::services::herdr_launch::{
            HERDR_NOT_ADMITTED, force_launch_gate, o_store_for_test,
            readiness_reads_on_this_thread as reads,
        };
        use crate::services::tui_o::cutover::test_override::{force_candidates, force_channels};
        use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::tests};
        use std::os::unix::fs::PermissionsExt;
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, _env) = tests::fixture_after_shared_test_env_lock();
        let executable = |name: &str, body: &str| {
            let path = root.path().join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        };
        executable(
            "tmux",
            "#!/bin/bash\nprintf '%s\\n' \"$*\" >> \"$AGENTDESK_ROOT_DIR/tmux.calls\"\n",
        );
        let claude = executable("claude", "#!/bin/bash\necho '2.1.0 (Claude Code)'\n");
        let path = format!(
            "{}:{}",
            root.path().display(),
            std::env::var("PATH").unwrap()
        );
        let _path = Guard::set_value_after_shared_test_env_lock("PATH", path.as_ref());
        let _bin = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CLAUDE_PATH", &claude);
        let (store, era) = o_store_for_test(root.path(), &[46]);
        let mut seeded = store.open_channel(&era, 46).unwrap().unwrap();
        seeded.set_binding_checkpoint(3).unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        let dir = root.path().to_str().unwrap();
        let run = |tmux: &str, channel| {
            launch(
                tmux,
                dir,
                root.path(),
                id,
                None,
                None,
                "".into(),
                false,
                "",
                Some(channel),
            )
        };
        let created = |tmux: &str| {
            let calls = std::fs::read_to_string(root.path().join("tmux.calls")).unwrap_or_default();
            calls.contains(&format!("new-session -d -s {tmux}"))
        };

        let _ready_channel = force_channels(&[(46, ClaudeTui), (47, ClaudeTui)]);
        let _writer = force_launch_gate(false, Some(true));
        run("AgentDesk-claude-gate-off", 46).expect("an unselected channel stays on tmux");
        assert!(created("AgentDesk-claude-gate-off"));
        assert_eq!(
            reads(),
            0,
            "no readiness is read while nothing selects Herdr"
        );

        let _selected = force_launch_gate(true, Some(true));
        let refused = run("AgentDesk-claude-gate-ready", 46);
        assert_eq!(refused.err().as_deref(), Some(HERDR_NOT_ADMITTED));
        assert!(
            !created("AgentDesk-claude-gate-ready"),
            "the Herdr branch creates no tmux"
        );
        assert_eq!(reads(), 1);

        let _pending = force_candidates(&[(46, ClaudeTui)]);
        run("AgentDesk-claude-gate-pending", 46).expect("an unadopted channel stays on tmux");
        assert!(created("AgentDesk-claude-gate-pending"));
        drop(_pending);
        run("AgentDesk-claude-gate-no-store", 47).expect("a channel without a store stays on tmux");
        assert!(created("AgentDesk-claude-gate-no-store"));
        assert_eq!(reads(), 3);
    }
}

#[cfg(all(test, unix))]
mod herdr_env_tests {
    use std::collections::BTreeMap;

    /// The fake provider's final environment and argv, keyed by the tag its runner set.
    fn child_env(root: &std::path::Path, tag: &str) -> (BTreeMap<String, String>, Vec<String>) {
        let read = |ext: &str| std::fs::read_to_string(root.join(format!("{tag}.{ext}"))).unwrap();
        let env = read("env")
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        (env, read("args").lines().map(str::to_string).collect())
    }

    #[test]
    fn herdr_launch_child_loses_the_herdr_pane_env_while_tmux_keeps_its_launch_env() {
        use super::{prepare_and_create_claude_tui_session, prepare_claude_herdr_launch};
        use crate::config::TestEnvVarGuard as Guard;
        use crate::services::herdr_launch::HERDR_PANE_ENV;
        use crate::services::tui_prompt_dedupe::binding_context::{PreparedIncarnation, tests};
        use std::os::unix::fs::PermissionsExt;
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let _lock = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let (root, _env) = tests::fixture_after_shared_test_env_lock();
        let executable = |name: &str, body: &str| {
            let path = root.path().join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        };
        executable("tmux", "#!/bin/bash\nexit 0\n");
        let dump = root.path().display();
        let claude = executable(
            "claude",
            &format!(
                "#!/bin/bash\n[ \"$1\" = --version ] && echo '2.1.0 (Claude Code)' && exit 0\n\
                 env > {dump}/$CHILD_TAG.env\nprintf '%s\\n' \"$@\" > {dump}/$CHILD_TAG.args\n"
            ),
        );
        let path = format!("{dump}:{}", std::env::var("PATH").unwrap());
        let _path = Guard::set_value_after_shared_test_env_lock("PATH", path.as_ref());
        let _bin = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CLAUDE_PATH", &claude);
        let home = root.path().join("claude-home");
        // The auth overlay sets a Herdr variable too: removal must follow every export.
        let auth = format!(
            "export CLAUDE_CONFIG_DIR={}\nexport HERDR_SOCKET_PATH=/overlay/herdr.sock\n",
            home.display()
        );
        let id = "11111111-1111-4111-8111-111111111111";
        let system_prompt = "keep this\nexec rm -rf /tmp/never";
        // What Herdr hands every process in the pane, the launch command included.
        let run = |tag: &str, command: &str| {
            let status = std::process::Command::new("bash")
                .args(["-c", command])
                .current_dir(root.path())
                .envs(HERDR_PANE_ENV.map(|key| (key, format!("pane-{key}"))))
                .env("CHILD_TAG", tag)
                .status()
                .unwrap();
            assert!(status.success(), "{tag}: {status}");
            child_env(root.path(), tag)
        };

        let prepared = PreparedIncarnation::prepare(
            "claude",
            "AgentDesk-claude-herdr-env",
            None,
            Some(id),
            false,
        )
        .unwrap();
        let herdr = prepare_claude_herdr_launch(
            &prepared,
            root.path(),
            id,
            Some(system_prompt),
            None,
            "http://127.0.0.1:1".into(),
            &auth,
        )
        .unwrap();
        assert_eq!(herdr.cwd, root.path());
        let (env, args) = run("herdr", &herdr.command);
        let leaked: Vec<_> = env.keys().filter(|key| key.starts_with("HERDR_")).collect();
        assert!(
            leaked.is_empty(),
            "Herdr pane variables reached the provider: {leaked:?}"
        );
        let binding = prepared.path.display().to_string();
        assert_eq!(env.get("AGENTDESK_BINDING_CONTEXT"), Some(&binding));
        assert_eq!(
            env.get("CLAUDE_CONFIG_DIR"),
            Some(&home.display().to_string())
        );
        assert_eq!(env.get("PATH"), Some(&path));
        assert_eq!(
            env.get("CLAUDE_CODE_RESUME_PROMPT").map(String::as_str),
            Some("_")
        );
        let settings = args
            .iter()
            .position(|arg| arg == "--settings")
            .map(|at| &args[at + 1]);
        assert!(
            settings.is_some_and(|hooks| std::path::Path::new(hooks).is_file()),
            "the AgentDesk relay hook settings stay on the command line: {args:?}"
        );
        assert!(
            args.windows(2).any(|pair| pair == ["--session-id", id]),
            "{args:?}"
        );
        assert!(
            args.join("\n").contains(system_prompt),
            "an exec line inside an argument is left alone: {args:?}"
        );

        let tmux = "AgentDesk-claude-tmux-env";
        prepare_and_create_claude_tui_session(
            tmux,
            &dump.to_string(),
            root.path(),
            id,
            None,
            None,
            "http://127.0.0.1:1".into(),
            false,
            &auth,
            None,
        )
        .unwrap();
        let script = crate::services::tmux_common::session_temp_path(
            tmux,
            crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        );
        let (env, _) = run("tmux", &format!("bash {script}"));
        let kept: Vec<_> = HERDR_PANE_ENV
            .iter()
            .filter_map(|key| env.get(*key))
            .collect();
        assert_eq!(
            kept,
            [
                "pane-HERDR_ENV",
                "pane-HERDR_PANE_ID",
                "pane-HERDR_BIN_PATH",
                "/overlay/herdr.sock"
            ],
            "the tmux launch environment is left as it was"
        );
        assert!(env.contains_key("AGENTDESK_BINDING_CONTEXT"));
        assert_eq!(
            env.get("CLAUDE_CONFIG_DIR"),
            Some(&home.display().to_string())
        );
    }
}
