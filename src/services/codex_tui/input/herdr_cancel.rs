//! Herdr `/stop` judges a Codex turn from an ANSI read, where a dim placeholder is no draft.

use super::composer_status::compact_body_is_fully_dim;
use super::{
    PROMPT_READY_SCAN_LINES, active_composer_visible_prompt_draft_in_pane,
    pane_has_codex_interactive_modal_in_pane, recent_codex_active_turn_marker,
    recent_has_codex_compact_composer, strip_ansi_escape_sequences,
};

/// Running text alone is insufficient when a modal or prompt draft owns the composer; only the
/// active compact `›` row with a wholly dim body is Codex's placeholder rather than a draft.
pub(crate) fn herdr_turn_in_progress(pane_with_escapes: &str) -> bool {
    let plain = strip_ansi_escape_sequences(pane_with_escapes);
    recent_codex_active_turn_marker(&plain).is_some()
        && !pane_has_codex_interactive_modal_in_pane(&plain)
        && (active_composer_visible_prompt_draft_in_pane(&plain).is_none()
            || active_compact_row_is_dim(pane_with_escapes, &plain))
}

/// The raw row behind the active compact composer, tied by index and its own plain text.
fn active_compact_row_is_dim(raw: &str, plain: &str) -> bool {
    let rows: Vec<(usize, &str)> = plain
        .lines()
        .map(str::trim_end)
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .collect();
    let recent: Vec<&str> = rows
        .iter()
        .rev()
        .take(PROMPT_READY_SCAN_LINES)
        .map(|(_, line)| *line)
        .collect();
    let Some(&(at, row)) = rows.len().checked_sub(2).and_then(|at| rows.get(at)) else {
        return false;
    };
    recent_has_codex_compact_composer(&recent)
        && raw.lines().nth(at).is_some_and(|line| {
            strip_ansi_escape_sequences(line).trim_end() == row && compact_body_is_fully_dim(line)
        })
}
