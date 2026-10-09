//! Pane gate: the existing readiness predicates plus a raw modal veto, failing closed.

use crate::services::claude_tui::prompt_readiness::normalize_prompt_readiness_panel_in_capture;
use crate::services::claude_tui::startup_dialog::detect_claude_startup_dialog;
use crate::services::claude_tui::{busy_inject, composer_lock::DraftSighting};
use crate::services::codex_tui::input::{
    prompt_readiness_from_ansi_pane, strip_ansi_escape_sequences, submission_draft_in_pane,
    submission_modal_in_pane,
};
use crate::services::tmux_common::{
    tmux_capture_indicates_claude_tui_busy, tmux_capture_indicates_claude_tui_exact_empty_composer,
    tmux_capture_indicates_claude_tui_interactive_modal,
    tmux_capture_indicates_claude_tui_mcp_auth_required,
    tmux_capture_indicates_claude_tui_prompt_draft,
    tmux_capture_indicates_claude_tui_ready_for_input,
};
use crate::services::tui_o::shadow::ShadowProvider;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneVerdict {
    Ready,
    Modal,
    /// Busy, a draft, or any screen not positively recognized as an empty composer.
    NotReady,
}

pub fn judge_pane(provider: ShadowProvider, capture: &str) -> PaneVerdict {
    #[cfg(test)]
    if super::super::transition::mutant("draft_veto") && capture.contains("foreign draft") {
        return PaneVerdict::Ready;
    }
    match provider {
        ShadowProvider::Claude => judge_claude(capture),
        ShadowProvider::Codex => judge_codex(capture),
    }
}

fn judge_claude(capture: &str) -> PaneVerdict {
    let plain = strip_ansi_escape_sequences(capture);
    let pane = normalize_prompt_readiness_panel_in_capture(&plain);
    if detect_claude_startup_dialog(&pane).is_some()
        || tmux_capture_indicates_claude_tui_interactive_modal(&pane)
        || tmux_capture_indicates_claude_tui_mcp_auth_required(&pane)
    {
        return PaneVerdict::Modal;
    }
    let ready = !tmux_capture_indicates_claude_tui_prompt_draft(&pane)
        && (tmux_capture_indicates_claude_tui_exact_empty_composer(&pane)
            || claude_measured_composer_empty(capture))
        && claude_composer_empty_when_present(capture)
        && tmux_capture_indicates_claude_tui_ready_for_input(&pane)
        && !tmux_capture_indicates_claude_tui_busy(&pane);
    if ready {
        PaneVerdict::Ready
    } else {
        PaneVerdict::NotReady
    }
}

fn judge_codex(capture: &str) -> PaneVerdict {
    let (composer_marker_detected, prompt_draft_detected, _) =
        prompt_readiness_from_ansi_pane(capture);
    let plain = strip_ansi_escape_sequences(capture);
    if submission_modal_in_pane(&plain) {
        return PaneVerdict::Modal;
    }
    if composer_marker_detected && !prompt_draft_detected {
        PaneVerdict::Ready
    } else {
        PaneVerdict::Modal
    }
}

// Only the bottom composer body can prove our paste; scrollback and chrome cannot.
pub(crate) fn own_draft(
    provider: ShadowProvider,
    capture: &str,
    frame: &str,
    pre_empty: bool,
) -> bool {
    #[cfg(test)]
    if super::super::transition::mutant("pre_empty") {
        return true;
    }
    if !pre_empty {
        return false;
    }
    let plain = strip_ansi_escape_sequences(capture);
    if provider == ShadowProvider::Codex {
        let (empty_marker, has_draft, _) = prompt_readiness_from_ansi_pane(capture);
        if empty_marker && !has_draft {
            return false;
        }
        let Some(draft) = submission_draft_in_pane(&plain) else {
            return false;
        };
        let folded = draft
            .strip_prefix("[Pasted Content ")
            .and_then(|count| count.strip_suffix(" chars]"))
            .is_some_and(|count| !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()));
        return draft == frame && !folded && !submission_modal_in_pane(&plain);
    }
    if !claude_composer_not_known_empty(capture) {
        return false;
    }
    let Some(rows) = claude_composer_rows(&plain) else {
        return false;
    };
    let body = rows.join("\n");
    if body == frame {
        return true;
    }
    #[cfg(test)]
    if super::super::transition::mutant("folded_unknown") {
        return false;
    }
    let Some(rest) = body.strip_prefix("[Pasted text #") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let suffix = &rest[digits..];
    let newlines = frame.bytes().filter(|b| *b == b'\n').count();
    if newlines == 0 {
        return frame.chars().count() > 800 && suffix == "]";
    }
    #[cfg(test)]
    if super::super::transition::mutant("folded_k") {
        return true;
    }
    suffix == format!(" +{newlines} lines]")
}

/// The composer shows exactly `rows`, as Claude wraps a paste; nothing folded or typed besides.
pub(crate) fn own_wrapped_draft(capture: &str, rows: &[String]) -> bool {
    let plain = strip_ansi_escape_sequences(capture);
    claude_composer_not_known_empty(capture)
        && claude_composer_rows(&plain)
            .is_some_and(|shown| shown.into_iter().eq(rows.iter().map(String::as_str)))
}

// Only the existing measured raw layout can identify a native hint as an empty composer.
fn claude_measured_composer_empty(capture: &str) -> bool {
    busy_inject::draft_sighting(capture) == DraftSighting::Settled
}

// An empty field cannot prove our payload even when its native hint matches the frame.
fn claude_composer_not_known_empty(capture: &str) -> bool {
    busy_inject::composer_owner(capture) != busy_inject::ComposerOwner::Empty
}

/// A visible composer must parse as exactly empty; a busy-only capture has no marker.
pub(crate) fn claude_composer_empty_when_present(capture: &str) -> bool {
    let plain = strip_ansi_escape_sequences(capture);
    !plain.lines().any(claude_prompt_row)
        || claude_composer_rows(&plain).is_some_and(|rows| {
            rows.len() == 1 && (rows[0].is_empty() || claude_measured_composer_empty(capture))
        })
}

// The bottom Claude composer's rows, prompt and continuation indent removed; none under a modal.
fn claude_composer_rows(plain: &str) -> Option<Vec<&str>> {
    if detect_claude_startup_dialog(plain).is_some()
        || tmux_capture_indicates_claude_tui_interactive_modal(plain)
        || tmux_capture_indicates_claude_tui_mcp_auth_required(plain)
    {
        return None;
    }
    let lines: Vec<_> = plain.lines().collect();
    let start = lines.iter().rposition(|line| claude_prompt_row(line))?;
    let end = (start + 1..lines.len()).find(|&i| claude_border_row(lines[i]))?;
    // An indented border-like row is pasted text, so the real border cannot be told apart.
    if lines[end].starts_with(char::is_whitespace) {
        return None;
    }
    // An indented marker may be a pasted continuation, so it proves no composer boundary.
    let first = lines[start].strip_prefix('❯')?;
    let first = first
        .strip_prefix(' ')
        .or_else(|| first.strip_prefix('\u{00a0}'))
        .unwrap_or(first);
    // Claude draws each continuation row two columns in, keeping the line's own leading spaces;
    // a row indented any other way is not our paste.
    let mut rows = vec![first];
    for line in &lines[start + 1..end] {
        rows.push(line.strip_prefix("  ").or(line.is_empty().then_some(""))?);
    }
    Some(rows)
}

/// A row the Claude composer reader takes for the prompt.
pub(crate) fn claude_prompt_row(line: &str) -> bool {
    line.trim_start().starts_with('❯')
}

/// A row the Claude composer reader takes for a border.
pub(crate) fn claude_border_row(line: &str) -> bool {
    let line = line.trim();
    line.chars().all(|c| c == '─') && line.len() >= 3
}
