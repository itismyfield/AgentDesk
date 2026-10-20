//! Stop-only evidence for a current foreground tool; shared readiness stays unchanged.

use crate::services::tmux_common as screen;

pub(super) fn foreground_tool_running(pane: &str) -> bool {
    if mutant("foreground_off")
        || !screen::tmux_capture_indicates_claude_tui_exact_empty_composer(pane)
    {
        return false;
    }
    let lines: Vec<_> = pane
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let Some(composer) = lines.iter().rposition(|line| line.starts_with('❯')) else {
        return false;
    };
    let Some(block) = lines[..composer]
        .iter()
        .rposition(|line| line.starts_with('⏺'))
    else {
        return false;
    };
    let block = &lines[block + 1..composer];
    let Some(detail) = block.first().and_then(|line| line.strip_prefix('⎿')) else {
        return false;
    };
    let command = detail.trim();
    let status = command.rsplit_once('(').map(|(_, status)| status);
    let running_status = status.is_some_and(|status| {
        status.ends_with(')')
            && status.contains('·')
            && status.split('·').next().is_some_and(|duration| {
                let duration = duration.trim();
                if !duration.ends_with(['s', 'm']) {
                    return false;
                }
                let duration = duration.trim_end_matches(['s', 'm']);
                !duration.is_empty() && duration.chars().all(|ch| ch.is_ascii_digit())
            })
    });
    command.starts_with("$ ")
        && running_status
        && block.get(1).is_some_and(|line| {
            line.starts_with('(')
                && line.contains("ctrl+b")
                && line.ends_with("to run in background)")
        })
        && block[2..]
            .iter()
            .all(|line| line.chars().all(|ch| ch == '─'))
}

pub(super) fn veto(pane: &str) -> bool {
    screen::tmux_capture_indicates_claude_tui_prompt_draft(pane)
        || screen::tmux_capture_indicates_claude_tui_interactive_modal(pane)
}

pub(super) use crate::services::provider::native_stop::mutant;
