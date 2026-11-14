//! Shows a local slash command's output row under the command row observed just before it in
//! the same session. The output row names no command; without that row it keeps its old note.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::injected_prompt_policy::{
    format_slash_command_control_note, format_slash_command_output_notice,
    is_local_command_output_row,
};

const LOCAL_COMMAND_STDOUT_KIND: &str = "local-command-stdout";
/// Both rows are written in one step; the window only bounds a command that printed nothing.
const PAIR_WINDOW: Duration = Duration::from_secs(10);

static LAST_LOCAL_COMMAND: LazyLock<Mutex<HashMap<String, (String, Instant)>>> =
    LazyLock::new(Default::default);

/// Ends the command slot of `tmux_session_name` at an output row, run before display dedupe so a
/// suppressed output row still ends it. Returns the command a kind-less output row is shown under.
pub(super) fn take_output_command(
    tmux_session_name: &str,
    kind: &str,
    raw: &str,
) -> Option<String> {
    if kind != LOCAL_COMMAND_STDOUT_KIND && !is_local_command_output_row(raw) {
        return None;
    }
    let mut last = LAST_LOCAL_COMMAND
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    last.retain(|_, (_, seen_at)| seen_at.elapsed() < PAIR_WINDOW);
    let command = last.remove(tmux_session_name).map(|(command, _)| command);
    // An output row with a command kind of its own (`/compact`'s "Compacted …") shows that kind.
    command.filter(|_| kind == LOCAL_COMMAND_STDOUT_KIND)
}

/// The note for one local-only slash-control row of `tmux_session_name`; `paired` is what
/// [`take_output_command`] returned for it. Only a command row names the next output's command.
pub(super) fn local_slash_control_note(
    tmux_session_name: &str,
    kind: &str,
    raw: &str,
    paired: Option<String>,
) -> String {
    if kind.starts_with('/') && !is_local_command_output_row(raw) {
        let mut last = LAST_LOCAL_COMMAND
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        last.retain(|_, (_, seen_at)| seen_at.elapsed() < PAIR_WINDOW);
        last.insert(
            tmux_session_name.to_string(),
            (kind.to_string(), Instant::now()),
        );
    }
    paired
        .and_then(|command| format_slash_command_output_notice(&command, raw))
        .unwrap_or_else(|| format_slash_command_control_note(tmux_session_name, kind, raw))
}

#[cfg(test)]
#[path = "slash_command_output_tests.rs"]
mod tests;
