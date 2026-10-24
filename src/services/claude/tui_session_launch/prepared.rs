//! Durable script preparation ends before the caller creates a pane.
use super::*;

pub(super) struct Prepare<'a> {
    pub tmux_session_name: &'a str,
    pub working_dir_path: &'a std::path::Path,
    pub resolved_session_id: &'a str,
    pub system_prompt: Option<&'a str>,
    pub model_override: Option<&'a str>,
    pub hook_endpoint: String,
    pub resume: bool,
    pub auth_env_lines: &'a str,
    pub channel_id: Option<u64>,
}

pub(super) fn prepare(
    input: Prepare<'_>,
    prepared_session_files: &mut Option<
        crate::services::claude_tui::session::ClaudeTuiSessionFiles,
    >,
) -> Result<(PreparedIncarnation, String), String> {
    let Prepare {
        tmux_session_name,
        working_dir_path,
        resolved_session_id,
        system_prompt,
        model_override,
        hook_endpoint,
        resume,
        auth_env_lines,
        channel_id,
    } = input;
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
    *prepared_session_files = Some(session_files);
    let script = std::fs::read_to_string(&launch_script_path)
        .map_err(|error| format!("read Claude TUI launch script: {error}"))?;
    let script = script.replacen(
        "#!/bin/bash\n",
        &format!("#!/bin/bash\n{}{auth_env_lines}", prepared.env_lines()),
        1,
    );
    std::fs::write(&launch_script_path, script)
        .map_err(|error| format!("update Claude TUI launch script: {error}"))?;
    let command = format!(
        "bash {}",
        shell_escape(&launch_script_path.display().to_string())
    );
    Ok((prepared, command))
}
