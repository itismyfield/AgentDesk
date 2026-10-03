//! Claude TUI launch-script parsing, and the launch transcript and binding a rehydrate derives
//! from it.

use super::*;

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ClaudeTuiLaunchInfo {
    pub(super) working_dir: PathBuf,
    pub(super) session_id: String,
    pub(super) binding_context_path: Option<PathBuf>,
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
    let launch = claude_tui_launch_info(tmux_session_name)?;
    transcript_for_launch(&launch, home)
}

#[cfg(unix)]
fn claude_tui_launch_info(tmux: &str) -> Option<ClaudeTuiLaunchInfo> {
    let launch_script_path = crate::services::tmux_common::resolve_session_temp_path(
        tmux,
        crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
    )?;
    parse_claude_tui_launch_script(Path::new(&launch_script_path)).ok()
}

#[cfg(unix)]
fn transcript_for_launch(
    launch: &ClaudeTuiLaunchInfo,
    home: Option<&Path>,
) -> Option<crate::services::tui_prompt_dedupe::pending::LaunchTranscript> {
    let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        &launch.working_dir,
        &launch.session_id,
        home,
    )
    .ok()?;
    let session_id = launch.session_id.clone();
    Some(
        crate::services::tui_prompt_dedupe::pending::LaunchTranscript {
            session_id,
            transcript,
        },
    )
}

#[cfg(unix)]
fn claude_rehydrate_home() -> Option<PathBuf> {
    #[cfg(test)]
    {
        super::rehydration::claude_pass_tests::claude_home()
    }
    #[cfg(not(test))]
    {
        None
    }
}

#[cfg(unix)]
pub(super) fn claude_launch_transcript(
    tmux: &str,
) -> Option<crate::services::tui_prompt_dedupe::pending::LaunchTranscript> {
    claude_tui_launch_transcript(tmux, claude_rehydrate_home().as_deref())
}

#[cfg(unix)]
pub(super) fn rehydrated_claude_tui_binding_for_tmux_session(
    tmux: &str,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    claude_launch_observation(tmux).0
}

/// The selector and its immutable context path come from one launch-script read.
#[cfg(unix)]
pub(super) fn claude_launch_observation(
    tmux: &str,
) -> (
    Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
    Option<PathBuf>,
) {
    let Some(launch) = claude_tui_launch_info(tmux) else {
        return (None, None);
    };
    let binding = transcript_for_launch(&launch, claude_rehydrate_home().as_deref())
        .filter(|launch| launch.transcript.exists())
        .map(|launch| claude_tui_rehydrated_binding(&launch.session_id, &launch.transcript));
    (binding, launch.binding_context_path)
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
    let mut binding_context_path = None;
    for line in script.lines() {
        let words = shell_words_from_line(line.trim());
        if words.first().is_some_and(|word| word == "export") {
            if let Some(path) = words
                .iter()
                .skip(1)
                .find_map(|word| word.strip_prefix("AGENTDESK_BINDING_CONTEXT="))
            {
                binding_context_path = (!path.is_empty()).then(|| PathBuf::from(path));
            }
            continue;
        }
        if words.first().is_some_and(|word| word == "unset")
            && words
                .iter()
                .skip(1)
                .any(|word| word == "AGENTDESK_BINDING_CONTEXT")
        {
            binding_context_path = None;
            continue;
        }
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
        binding_context_path,
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
                binding_context_path: None,
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_launch_selector_keeps_its_context_when_the_live_nonce_changes() {
        use crate::services::tui_prompt_dedupe::binding_context::{
            CapturedContext, HookBindingEnvelope, PreparedIncarnation, SpawnNonceMarker,
            observe_spawn_nonce_marker,
            tests::{fixture, prepared},
        };
        let (_root, _env) = fixture();
        let previous = prepared();
        let script = format!(
            "{}cd '/tmp/project'\\''s dir'\nexec 'claude' '--session-id' 'launch-session'\n",
            previous.env_lines(),
        );
        let parsed = parse_claude_tui_launch_script_content(&script).unwrap();
        assert_eq!(parsed.session_id, "launch-session");
        assert_eq!(parsed.binding_context_path.as_ref(), Some(&previous.path));
        let mut next = previous.context.clone();
        next.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
        let next = PreparedIncarnation::create(next).unwrap();
        let marker = crate::services::tmux_common::session_temp_path(
            &previous.context.tmux_session,
            "spawn_nonce",
        );
        std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, &next.context.execution_nonce).unwrap();
        let captured = HookBindingEnvelope::capture_from_env("claude", |key| {
            (key == "AGENTDESK_BINDING_CONTEXT").then(|| {
                parsed
                    .binding_context_path
                    .clone()
                    .unwrap()
                    .into_os_string()
            })
        });
        assert_eq!(
            captured.context,
            CapturedContext::Captured(previous.context.clone())
        );
        assert_eq!(
            observe_spawn_nonce_marker(&previous.context.tmux_session),
            SpawnNonceMarker::Known(next.context.execution_nonce),
        );
    }
}
