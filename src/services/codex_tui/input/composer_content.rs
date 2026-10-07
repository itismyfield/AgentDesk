//! The Herdr turn's reading of Codex's active composer, taken whole so a draft after the cursor
//! or on another line is seen; the tmux paths keep their own draft reader.

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

/// What the active composer holds, read whole: text after its cursor and on its other lines
/// counts too. Only `Empty` shows that a write lands on nothing a person typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerContent {
    Empty,
    Draft,
    /// No composer layout this reader knows, or not exactly one cursor in it.
    Unread,
}

/// Placeholders as Codex draws them, right after the cursor of an empty input.
const DRAWN_PLACEHOLDERS: &[&str] = &[
    "Send a message",
    "Send a message...",
    "Send a message…",
    "Type / for commands",
    "Type a message",
    "Message",
];

/// The Herdr turn's reading of the bottom-most composer, box or compact; `Empty` only for a blank
/// input or a placeholder right after its one cursor, and any other screen is a draft or unread.
pub(crate) fn active_composer_content_in_pane(pane: &str) -> ComposerContent {
    let lines: Vec<&str> = pane.lines().map(str::trim_end).collect();
    let recent: Vec<usize> = (0..lines.len())
        .rev()
        .filter(|&i| !lines[i].trim().is_empty())
        .take(PROMPT_READY_SCAN_LINES)
        .collect();
    let rows: Vec<&str> = recent.iter().map(|&i| lines[i]).collect();
    match herdr_composer_body(&rows) {
        Some(body) => boxed_content(body),
        None => compact_content(&lines, &recent, &rows),
    }
}

fn boxed_content(body: &[&str]) -> ComposerContent {
    let (mut rows, mut cursors) = (Vec::new(), 0);
    for line in body {
        let trimmed = line.trim();
        let Some(inner) = trimmed.strip_prefix('│').and_then(|l| l.strip_suffix('│')) else {
            return ComposerContent::Unread;
        };
        cursors += inner.matches('▌').count();
        let inner = inner.trim_matches(blank);
        if !inner.is_empty() {
            rows.push(inner);
        }
    }
    match (cursors, rows.as_slice()) {
        (1, [row]) => cursor_row_content(row),
        (1, _) => ComposerContent::Draft,
        _ => ComposerContent::Unread,
    }
}

/// A compact composer is one unindented `›` row right above the status row, the only `›` row
/// in view, with a blank row above it; Codex indents a draft's later rows below its first.
fn compact_content(lines: &[&str], recent: &[usize], rows: &[&str]) -> ComposerContent {
    let [status, prompt, ..] = rows else {
        return ComposerContent::Unread;
    };
    let blank_above = recent[1]
        .checked_sub(1)
        .is_none_or(|above| lines[above].trim_matches(blank).is_empty());
    let prompts = rows
        .iter()
        .filter(|row| row.trim_start_matches(blank).starts_with('›'))
        .count();
    let Some(input) = prompt.strip_prefix('›') else {
        return ComposerContent::Unread;
    };
    if !line_is_status(status) || prompts != 1 || !blank_above {
        return ComposerContent::Unread;
    }
    match input.trim_matches(blank) {
        "" => ComposerContent::Empty,
        input => cursor_row_content(input),
    }
}

fn line_is_status(line: &str) -> bool {
    line_is_codex_compact_status_line(line) || line_is_codex_fast_context_status(line)
}

/// A trimmed input row: empty when it is the cursor alone or the cursor then a drawn placeholder.
fn cursor_row_content(row: &str) -> ComposerContent {
    match row.strip_prefix('▌') {
        Some(rest) if rest.is_empty() || DRAWN_PLACEHOLDERS.contains(&rest) => {
            ComposerContent::Empty
        }
        _ => ComposerContent::Draft,
    }
}

fn blank(ch: char) -> bool {
    ch.is_whitespace() || ch == '\u{00a0}'
}

/// The body, bottom first, of the one `╭─…─╮`/`╰─…─╯` box with only `│…│` rows inside, no border
/// above it, and only footer hint and status rows below it.
fn herdr_composer_body<'r, 'a>(recent: &'r [&'a str]) -> Option<&'r [&'a str]> {
    let footer_idx = recent
        .iter()
        .take(FOOTER_HINT_BOTTOM_WINDOW)
        .position(|line| line_is_codex_footer_hint(line))?;
    let bottom_idx = recent
        .iter()
        .take(COMPOSER_EDGE_BOTTOM_WINDOW)
        .position(|line| line_is_border_like(line))?;
    if !line_is_box_rule(recent[bottom_idx], '╰', '╯')
        || footer_idx > bottom_idx
        || bottom_idx - footer_idx > COMPOSER_FOOTER_ADJACENCY_LINES
        || !recent[..bottom_idx]
            .iter()
            .all(|line| line_is_codex_footer_hint(line) || line_is_status(line))
    {
        return None;
    }
    let body_start = bottom_idx + 1;
    let top_idx = body_start
        + recent[body_start..].iter().position(|line| {
            let trimmed = line.trim();
            !(trimmed.starts_with('│') && trimmed.ends_with('│'))
        })?;
    if !line_is_box_rule(recent[top_idx], '╭', '╮')
        || recent[top_idx + 1..]
            .iter()
            .any(|line| line_is_border_like(line))
    {
        return None;
    }
    Some(&recent[body_start..top_idx])
}

/// A box edge from `left` to `right` drawn with `─` only.
fn line_is_box_rule(line: &str, left: char, right: char) -> bool {
    line.trim()
        .strip_prefix(left)
        .and_then(|rest| rest.strip_suffix(right))
        .is_some_and(|rule| !rule.is_empty() && rule.chars().all(|ch| ch == '─'))
}

/// Any row that could be some box's edge.
fn line_is_border_like(line: &str) -> bool {
    let trimmed = line.trim();
    line_is_codex_composer_edge(line)
        || trimmed.starts_with(['╭', '╰'])
        || trimmed.ends_with(['╮', '╯'])
}
