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
}
