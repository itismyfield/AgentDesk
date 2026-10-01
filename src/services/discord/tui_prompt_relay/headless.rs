//! Keeps a Claude TUI pane's relay binding off headless SDK (`claude -p`) transcripts that share
//! its project directory.
#![cfg(unix)]

use super::claude_idle_runtime::{claude_tui_launch_context, other_session_claimed_transcripts};
use super::launch_script::claude_tui_rehydrated_binding;
use super::*;

/// The launch binding to rehydrate: a launch script naming a headless SDK transcript yields the
/// pane's TUI transcript instead, or nothing when there is none.
pub(super) fn tui_binding(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    fresh: Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    let fresh = fresh?;
    let launch_path = Path::new(&fresh.output_path);
    if !crate::services::claude_tui::transcript_tail::claude_transcript_is_headless_sdk(launch_path)
    {
        return Some(fresh);
    }
    let (path, session_id) =
        claude_tui_transcript_replacing_headless(shared, tmux_session_name, launch_path)?;
    Some(claude_tui_rehydrated_binding(session_id.as_deref()?, &path))
}

/// The newest TUI transcript under the launch cwd since launch that no other live session claims.
pub(super) fn freshest_unclaimed_claude_transcript(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
) -> Option<(PathBuf, Option<String>)> {
    let claimed_by_other_sessions = other_session_claimed_transcripts(shared, tmux_session_name);
    claude_tui_launch_context(tmux_session_name)
        .and_then(|(cwd, launch_mtime)| {
            crate::services::claude_tui::transcript_tail::latest_claude_transcript_for_cwd(
                &cwd,
                launch_mtime,
                None,
                &claimed_by_other_sessions,
            )
        })
        .map(|path| {
            let session_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string);
            (path, session_id)
        })
}

/// The pane's TUI transcript in place of a headless SDK one it was bound to, also written back
/// to the launch artifacts so a restart does not rebind the headless file. `None` when none exists.
pub(super) fn claude_tui_transcript_replacing_headless(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    headless_path: &Path,
) -> Option<(PathBuf, Option<String>)> {
    let (path, session_id) = freshest_unclaimed_claude_transcript(shared, tmux_session_name)?;
    if let Some(Err(error)) = session_id.as_deref().map(|id| {
        crate::services::claude_tui::session::persist_claude_continuation_session(
            tmux_session_name,
            id,
        )
    }) {
        tracing::error!(
            tmux_session_name,
            error,
            "failed to persist the Claude TUI session replacing a headless transcript binding"
        );
    }
    tracing::warn!(
        tmux_session_name,
        headless_path = %headless_path.display(),
        transcript_path = %path.display(),
        "replaced a headless SDK transcript binding with the pane's Claude TUI transcript"
    );
    Some((path, session_id))
}
