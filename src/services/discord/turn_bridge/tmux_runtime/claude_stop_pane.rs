//! Stop-only pane evidence: a running turn's own chrome rows and a current foreground tool.
//! Shared readiness stays unchanged.

use crate::services::tmux_common as screen;

pub(super) fn foreground_tool_running(pane: &str) -> bool {
    if !screen::tmux_capture_indicates_claude_tui_exact_empty_composer(pane) {
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

/// The recent rows where Claude paints a running turn's chrome: left-edge rows, and the status
/// rows under the composer's bottom border. Indented rows above it are prompt, draft or reply text.
fn chrome_rows(pane: &str) -> Vec<&str> {
    let rows: Vec<&str> = pane.lines().filter(|row| !row.trim().is_empty()).collect();
    let skip = rows
        .len()
        .saturating_sub(screen::CLAUDE_TUI_READINESS_SCAN_LINES);
    let recent = &rows[skip..];
    let edge = |row: &str| !row.starts_with(char::is_whitespace);
    let border = |row: &&str| edge(row) && row.trim_end().chars().all(|ch| ch == '─');
    let composer = recent.iter().rposition(|row| row.starts_with('❯'));
    let below = |at: usize| Some(at + recent[at..].iter().position(border)?);
    let status = composer.and_then(below).unwrap_or(recent.len());
    let rows = recent.iter().enumerate();
    let chrome = rows.filter(|(at, row)| *at > status || edge(row));
    chrome.map(|(_, row)| *row).collect()
}

/// Claude's own `esc to interrupt` offer, in the footer or on a spinner row: what an unread turn
/// stops on. Spinner chrome over the painted composer is not enough without the turn's identity.
pub(super) fn interrupt_offered(pane: &str) -> bool {
    !veto(pane) && screen::tmux_recent_lines_show_claude_tui_interrupt_chrome(&chrome_rows(pane))
}

/// A token stop's evidence. A streaming tail keeps Escape over a draft or modal as it always
/// has; a pane that decides alone shares the veto.
pub(super) fn token_evidence(pane: &str, streaming: bool) -> (bool, bool, bool) {
    let (ready, active, draft) = legacy_evidence(pane);
    (ready, active && (streaming || !veto(pane)), draft)
}

pub(super) fn legacy_evidence(pane: &str) -> (bool, bool, bool) {
    // Only chrome a running turn paints counts, never the same words in a prompt or a reply; a
    // spinner frame also counts above the exactly empty composer Claude keeps painted meanwhile.
    let rows = chrome_rows(pane);
    let spinner = screen::tmux_capture_indicates_claude_tui_structured_spinner(&rows.join("\n"));
    let offered = screen::tmux_recent_lines_show_claude_tui_interrupt_chrome(&rows);
    let active = (screen::tmux_capture_indicates_claude_tui_actively_streaming(pane)
        && (spinner || offered))
        || (spinner && screen::tmux_capture_indicates_claude_tui_exact_empty_composer(pane));
    (
        screen::tmux_capture_indicates_claude_tui_ready_for_input(pane),
        active,
        screen::tmux_capture_indicates_claude_tui_prompt_draft(pane),
    )
}
