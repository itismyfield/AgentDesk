//! The Herdr turn's reading of Codex's compact composer, the only form current captures show,
//! and the boxed body finder the tmux draft reader keeps.

use super::{
    COMPOSER_EDGE_BOTTOM_WINDOW, COMPOSER_FOOTER_ADJACENCY_LINES, FOOTER_HINT_BOTTOM_WINDOW,
    PROMPT_READY_SCAN_LINES, line_is_codex_compact_status_line, line_is_codex_composer_edge,
    line_is_codex_fast_context_status, line_is_codex_footer_hint,
};

/// The body lines of the bottom-most boxed composer, bottom first; `None` unless its edges sit
/// right above the footer hint.
pub(super) fn boxed_composer_body<'r, 'a>(recent: &'r [&'a str]) -> Option<&'r [&'a str]> {
    let footer_idx = recent
        .iter()
        .take(FOOTER_HINT_BOTTOM_WINDOW)
        .position(|line| line_is_codex_footer_hint(line))?;
    let bottom_edge_idx = recent
        .iter()
        .take(COMPOSER_EDGE_BOTTOM_WINDOW)
        .position(|line| line_is_codex_composer_edge(line))?;
    if footer_idx > bottom_edge_idx
        || bottom_edge_idx - footer_idx > COMPOSER_FOOTER_ADJACENCY_LINES
    {
        return None;
    }
    let body_start = bottom_edge_idx + 1;
    let top_edge_offset = recent
        .iter()
        .skip(body_start)
        .position(|line| line_is_codex_composer_edge(line))?;
    Some(&recent[body_start..body_start + top_edge_offset])
}

/// What the active composer holds. Only `Empty` shows that a write lands on nothing a person typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerContent {
    Empty,
    Draft,
    /// Not one compact composer this reader can prove whole, such as any boxed screen.
    Unread,
}

/// The Herdr turn's composer reading: `Empty` only for a bare unindented `›` right above the
/// status row, the one `›` row on a screen of at most the scan window with no box corner on it.
pub(crate) fn active_composer_content_in_pane(pane: &str) -> ComposerContent {
    // Codex indents a draft's later rows, so that `›` row starts the composer only if Herdr
    // `pane.read` (capture -80, RecentUnwrapped, strip_ansi) keeps leading spaces and row breaks.
    let lines: Vec<&str> = pane.lines().map(str::trim_end).collect();
    let shown: Vec<usize> = (0..lines.len())
        .rev()
        .filter(|&i| !lines[i].trim_matches(blank).is_empty())
        .collect();
    let rows: Vec<&str> = shown.iter().map(|&i| lines[i]).collect();
    let (&[status, prompt, ..], &[_, prompt_at, ..]) = (rows.as_slice(), shown.as_slice()) else {
        return ComposerContent::Unread;
    };
    let whole = rows.len() <= PROMPT_READY_SCAN_LINES;
    let boxed = rows
        .iter()
        .any(|row| row.trim_start_matches(blank).starts_with(['╭', '╰']));
    let prompts = rows
        .iter()
        .filter(|row| row.trim_start_matches(blank).starts_with('›'))
        .count();
    let blank_above = prompt_at
        .checked_sub(1)
        .is_none_or(|above| lines[above].trim_matches(blank).is_empty());
    let Some(input) = prompt.strip_prefix('›') else {
        return ComposerContent::Unread;
    };
    if !whole || boxed || prompts != 1 || !blank_above || !line_is_status(status) {
        return ComposerContent::Unread;
    }
    match input.trim_matches(blank) {
        "" => ComposerContent::Empty,
        _ => ComposerContent::Draft,
    }
}

fn line_is_status(line: &str) -> bool {
    line_is_codex_compact_status_line(line) || line_is_codex_fast_context_status(line)
}

fn blank(ch: char) -> bool {
    ch.is_whitespace() || ch == '\u{00a0}'
}
