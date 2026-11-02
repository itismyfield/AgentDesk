//! Durable script preparation ends before the caller creates a pane.
use super::*;

pub(super) struct Prepare<'a> {
    pub tmux_session_name: &'a str,
    pub working_dir_path: &'a std::path::Path,
    pub resolved_session_id: &'a str,
    pub system_prompt: Option<&'a str>,
    pub launch: crate::services::claude_tui::session::ClaudeTuiLaunchSettings<'a>,
    pub hook_endpoint: String,
    pub resume: bool,
    pub auth_env_lines: &'a str,
    /// The account's own Claude home, whose projects dir the launched child writes under.
    pub claude_home: Option<&'a std::path::Path>,
    pub channel_id: Option<u64>,
}

pub(super) fn prepare(
    input: Prepare<'_>,
    prepared_session_files: &mut Option<
        crate::services::claude_tui::session::ClaudeTuiSessionFiles,
    >,
    mark_host: impl FnOnce(),
) -> Result<(PreparedIncarnation, String), String> {
    let Prepare {
        tmux_session_name,
        working_dir_path,
        resolved_session_id,
        system_prompt,
        launch,
        hook_endpoint,
        resume,
        auth_env_lines,
        claude_home,
        channel_id,
    } = input;
    let expected = Some(resolved_session_id);
    let prepared = match claude_home {
        Some(home) => PreparedIncarnation::prepare_at(
            "claude",
            tmux_session_name,
            channel_id,
            expected,
            resume,
            Some(home.join("projects")),
        ),
        None => {
            PreparedIncarnation::prepare("claude", tmux_session_name, channel_id, expected, resume)
        }
    }?;
    mark_host();
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
        model: launch.model.map(str::to_string),
        resume,
    };
    let prepare = crate::services::claude_tui::session::prepare_claude_tui_launch_with_settings;
    let session_files = prepare(&launch_config, launch.fast_mode, launch.effort)?;
    session_prompt::record(launch_config.system_prompt.as_deref(), resume);
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

/// A requested session whose transcript is gone, launched as a fresh session in its place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::claude) struct FreshSubstitution {
    pub requested: String,
    pub substituted: String,
    pub missing_transcript: std::path::PathBuf,
}

/// The launch's native session: the requested one when its transcript exists, otherwise a fresh
/// one that names the session it replaced.
pub(in crate::services::claude) fn resolve(
    working_dir: &std::path::Path,
    requested_session_id: Option<&str>,
    claude_home: Option<&std::path::Path>,
) -> Result<(ClaudeTuiSessionResolution, Option<FreshSubstitution>), String> {
    use crate::services::claude_tui::transcript_tail::claude_transcript_path;
    let session_id = match requested_session_id {
        Some(sid) if is_valid_session_id(sid) => sid.to_string(),
        Some(_) => return Err("Invalid session ID format".to_string()),
        None => uuid::Uuid::new_v4().to_string(),
    };
    let transcript_path = claude_transcript_path(working_dir, &session_id, claude_home)?;
    if requested_session_id.is_none() || transcript_path.exists() {
        let resume = requested_session_id.is_some();
        let resolution = ClaudeTuiSessionResolution {
            session_id,
            transcript_path,
            resume,
        };
        return Ok((resolution, None));
    }
    let fresh = uuid::Uuid::new_v4().to_string();
    let resolution = ClaudeTuiSessionResolution {
        transcript_path: claude_transcript_path(working_dir, &fresh, claude_home)?,
        session_id: fresh.clone(),
        resume: false,
    };
    let substitution = FreshSubstitution {
        requested: session_id,
        substituted: fresh,
        missing_transcript: transcript_path,
    };
    Ok((resolution, Some(substitution)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A requested session without a transcript reports which fresh session replaced it; a live
    /// transcript resumes and no request starts fresh, neither reporting a substitution.
    #[test]
    fn forced_fresh_reports_substitution() {
        let (cwd, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let resolve = |sid| resolve(cwd.path(), sid, Some(home.path())).unwrap();
        let stale = uuid::Uuid::new_v4().to_string();
        let (resolution, substitution) = resolve(Some(&stale));
        let substitution = substitution.expect("a missing transcript is a substitution");
        assert_eq!(substitution.requested, stale);
        assert_eq!(substitution.substituted, resolution.session_id);
        assert_ne!(resolution.session_id, stale);
        assert!(!resolution.resume);
        assert!(
            substitution
                .missing_transcript
                .ends_with(format!("{stale}.jsonl"))
        );
        std::fs::create_dir_all(substitution.missing_transcript.parent().unwrap()).unwrap();
        std::fs::write(&substitution.missing_transcript, "").unwrap();
        let (resumed, none) = resolve(Some(&stale));
        assert_eq!(
            (resumed.session_id, resumed.resume, none),
            (stale.clone(), true, None)
        );
        let (fresh, none) = resolve(None);
        assert_eq!((fresh.resume, none), (false, None));
    }
}
