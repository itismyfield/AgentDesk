//! The Herdr turn's reading of Codex's active composer, taken whole so a draft after the cursor
//! or on another line is seen; the tmux paths keep their own draft reader.

use super::{
    COMPOSER_EDGE_BOTTOM_WINDOW, COMPOSER_FOOTER_ADJACENCY_LINES, FOOTER_HINT_BOTTOM_WINDOW,
    PROMPT_READY_SCAN_LINES, codex_composer_placeholder_text, line_is_codex_composer_edge,
    line_is_codex_footer_hint, recent_has_codex_compact_composer,
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

/// The Herdr turn's composer reading.
pub(crate) fn active_composer_content_in_pane(pane: &str) -> ComposerContent {
    let recent: Vec<&str> = pane
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(PROMPT_READY_SCAN_LINES)
        .collect();
    if recent_has_codex_compact_composer(&recent) {
        let prompt = recent[1].trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
        return composer_text_content(prompt.trim_start_matches('›'));
    }
    let Some(body) = boxed_composer_body(&recent) else {
        return ComposerContent::Unread;
    };
    let (mut text, mut cursors) = (String::new(), 0);
    for line in body {
        let trimmed = line.trim();
        let Some(inner) = trimmed.strip_prefix('│').and_then(|l| l.strip_suffix('│')) else {
            return ComposerContent::Unread;
        };
        cursors += inner.matches('▌').count();
        text.push_str(&inner.replace('▌', " "));
        text.push('\n');
    }
    match cursors {
        1 => composer_text_content(&text),
        _ => ComposerContent::Unread,
    }
}

fn composer_text_content(text: &str) -> ComposerContent {
    let text = text.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{00a0}');
    if text.is_empty() || codex_composer_placeholder_text(text) {
        ComposerContent::Empty
    } else {
        ComposerContent::Draft
    }
}
