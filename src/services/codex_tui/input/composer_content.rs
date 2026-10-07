//! The Herdr turn's reading of Codex's active composer, taken whole so a draft after the cursor
//! or on another line is seen; the tmux paths keep their own draft reader.

use super::{
    COMPOSER_EDGE_BOTTOM_WINDOW, COMPOSER_FOOTER_ADJACENCY_LINES, FOOTER_HINT_BOTTOM_WINDOW,
    PROMPT_READY_SCAN_LINES, line_is_codex_composer_edge, line_is_codex_footer_hint,
    recent_has_codex_compact_composer,
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

/// The Herdr turn's composer reading: `Empty` only for a blank input or a placeholder drawn
/// right after its one cursor; any other text is a draft, and an uncertain layout is unread.
pub(crate) fn active_composer_content_in_pane(pane: &str) -> ComposerContent {
    let recent: Vec<&str> = pane
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(PROMPT_READY_SCAN_LINES)
        .collect();
    if recent_has_codex_compact_composer(&recent) {
        let prompt = recent[1].trim_matches(blank).trim_start_matches('›');
        return match prompt.trim_matches(blank) {
            "" => ComposerContent::Empty,
            prompt => cursor_row_content(prompt),
        };
    }
    let Some(body) = herdr_composer_body(&recent) else {
        return ComposerContent::Unread;
    };
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

/// The body of the one boxed composer right above the footer hint, bottom first: its edges are
/// `╰─…─╯` and `╭─…─╮`, every row between is `│…│`, and no other border sits above it.
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
