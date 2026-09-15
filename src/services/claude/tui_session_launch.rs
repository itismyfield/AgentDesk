//! Tui session launch.

use super::*;

/// Prepare the Claude TUI launch script and hosted tmux session.
/// Verbatim prep/create extraction: temp cleanup, owner/runtime markers, launch script, create_session; marker `?` exits precede cleanup, later failures keep original cleanup, success returns owner path.
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
) -> Result<String, String> {
    crate::services::tmux_common::cleanup_session_temp_files(tmux_session_name);
    write_tmux_owner_marker(tmux_session_name)?;
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        tmux_session_name,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )?;
    let owner_path = tmux_owner_path(tmux_session_name);
    let mut prepared_session_files = None;
    let launch_result = (|| -> Result<std::process::Output, String> {
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
        if !auth_env_lines.is_empty() {
            let script = std::fs::read_to_string(&session_files.launch_script_path)
                .map_err(|error| format!("read Claude TUI launch script: {error}"))?;
            let script = script.replacen(
                "#!/bin/bash\n",
                &format!("#!/bin/bash\n{auth_env_lines}"),
                1,
            );
            std::fs::write(&session_files.launch_script_path, script)
                .map_err(|error| format!("update Claude TUI auth launch script: {error}"))?;
        }
        let launch_script_path = session_files.launch_script_path.clone();
        prepared_session_files = Some(session_files);
        crate::services::platform::tmux::create_session(
            tmux_session_name,
            Some(working_dir),
            &format!(
                "bash {}",
                shell_escape(&launch_script_path.display().to_string())
            ),
        )
    })();
    let tmux_result = match launch_result {
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
    Ok(owner_path)
}
