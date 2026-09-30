//! Claude TUI launch-script parsing, and the launch transcript and binding a rehydrate derives
//! from it.

use super::*;

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ClaudeTuiLaunchInfo {
    pub(super) working_dir: PathBuf,
    pub(super) session_id: String,
}

#[cfg(unix)]
pub(super) fn parse_claude_tui_launch_script(path: &Path) -> Result<ClaudeTuiLaunchInfo, String> {
    let script = std::fs::read_to_string(path)
        .map_err(|error| format!("read Claude TUI launch script {}: {error}", path.display()))?;
    parse_claude_tui_launch_script_content(&script)
        .ok_or_else(|| format!("parse Claude TUI launch script {}", path.display()))
}

/// The launch session of `tmux_session_name` and where its transcript is, whether or not it exists.
#[cfg(unix)]
pub(super) fn claude_tui_launch_transcript(
    tmux_session_name: &str,
    home: Option<&Path>,
) -> Option<crate::services::tui_prompt_dedupe::pending::LaunchTranscript> {
    let launch_script_path = crate::services::tmux_common::resolve_session_temp_path(
        tmux_session_name,
        crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
    )?;
    let launch = parse_claude_tui_launch_script(Path::new(&launch_script_path)).ok()?;
    let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        &launch.working_dir,
        &launch.session_id,
        home,
    )
    .ok()?;
    let session_id = launch.session_id;
    Some(
        crate::services::tui_prompt_dedupe::pending::LaunchTranscript {
            session_id,
            transcript,
        },
    )
}

/// The binding a rehydrate registers for `session_id`, read from the transcript's current end.
#[cfg(unix)]
pub(super) fn claude_tui_rehydrated_binding(
    session_id: &str,
    transcript_path: &Path,
) -> crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
    crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: transcript_path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session_id.to_owned()),
        last_offset: claude_tui_rehydrate_start_offset(transcript_path),
        relay_last_offset: None,
    }
}

#[cfg(unix)]
fn parse_claude_tui_launch_script_content(script: &str) -> Option<ClaudeTuiLaunchInfo> {
    let mut working_dir: Option<PathBuf> = None;
    let mut session_id: Option<String> = None;
    for line in script.lines() {
        let words = shell_words_from_line(line.trim());
        if words.first().is_some_and(|word| word == "cd") {
            if let Some(dir) = words.get(1).filter(|value| !value.trim().is_empty()) {
                working_dir = Some(PathBuf::from(dir));
            }
            continue;
        }
        if !words.first().is_some_and(|word| word == "exec") {
            continue;
        }
        for pair in words.windows(2) {
            if matches!(pair[0].as_str(), "--session-id" | "--resume") && !pair[1].trim().is_empty()
            {
                session_id = Some(pair[1].clone());
                break;
            }
        }
    }
    Some(ClaudeTuiLaunchInfo {
        working_dir: working_dir?,
        session_id: session_id?,
    })
}

#[cfg(unix)]
fn shell_words_from_line(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut saw_word = false;
    let mut in_single = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
            saw_word = true;
            continue;
        }

        if ch.is_whitespace() {
            if saw_word {
                words.push(std::mem::take(&mut current));
                saw_word = false;
            }
            continue;
        }

        match ch {
            '\'' => {
                in_single = true;
                saw_word = true;
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    saw_word = true;
                }
            }
            _ => {
                current.push(ch);
                saw_word = true;
            }
        }
    }

    if saw_word {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn parses_claude_tui_launch_script_content() {
        let script = concat!(
            "#!/bin/bash\n",
            "cd '/tmp/project'\\''s dir'\n",
            "exec '/usr/local/bin/claude' '--dangerously-skip-permissions' '--session-id' '01234567-89ab-cdef-0123-456789abcdef' '--settings' '/tmp/settings.json'\n",
        );

        assert_eq!(
            parse_claude_tui_launch_script_content(script),
            Some(ClaudeTuiLaunchInfo {
                working_dir: PathBuf::from("/tmp/project's dir"),
                session_id: "01234567-89ab-cdef-0123-456789abcdef".to_string(),
            })
        );
    }
}
