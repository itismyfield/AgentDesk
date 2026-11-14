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

/// The note for one local-only slash-control row of `tmux_session_name`.
pub(super) fn local_slash_control_note(tmux_session_name: &str, kind: &str, raw: &str) -> String {
    let paired = {
        let mut last = LAST_LOCAL_COMMAND
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        last.retain(|_, (_, seen_at)| seen_at.elapsed() < PAIR_WINDOW);
        if kind == LOCAL_COMMAND_STDOUT_KIND {
            last.remove(tmux_session_name).map(|(command, _)| command)
        } else {
            // An output row can carry a command kind of its own (`/compact`'s "Compacted …");
            // only the command row names the command the next output belongs to.
            if kind.starts_with('/') && !is_local_command_output_row(raw) {
                last.insert(
                    tmux_session_name.to_string(),
                    (kind.to_string(), Instant::now()),
                );
            }
            None
        }
    };
    paired
        .and_then(|command| format_slash_command_output_notice(&command, raw))
        .unwrap_or_else(|| format_slash_command_control_note(tmux_session_name, kind, raw))
}

#[cfg(test)]
#[path = "slash_command_output_tests.rs"]
mod tests;
