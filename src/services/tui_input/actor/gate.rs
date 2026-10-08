//! Pane gate: the existing readiness predicates plus a raw modal veto, failing closed.

use crate::services::claude_tui::prompt_readiness::normalize_prompt_readiness_panel_in_capture;
use crate::services::claude_tui::startup_dialog::detect_claude_startup_dialog;
use crate::services::codex_tui::input::{
    PromptReadinessSnapshot, active_composer_visible_prompt_draft_in_pane,
    prompt_readiness_from_ansi_pane, steering_snapshot_decision, strip_ansi_escape_sequences,
};
use crate::services::tmux_common::{
    tmux_capture_indicates_claude_tui_busy, tmux_capture_indicates_claude_tui_exact_empty_composer,
    tmux_capture_indicates_claude_tui_interactive_modal,
    tmux_capture_indicates_claude_tui_mcp_auth_required,
    tmux_capture_indicates_claude_tui_prompt_draft,
    tmux_capture_indicates_claude_tui_ready_for_input,
};
use crate::services::tui_o::shadow::ShadowProvider;

// Codex steering judges modal wording over this many trailing lines.
const CODEX_MODAL_TAIL_LINES: usize = 24;

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
        && tmux_capture_indicates_claude_tui_exact_empty_composer(&pane)
        && tmux_capture_indicates_claude_tui_ready_for_input(&pane)
        && !tmux_capture_indicates_claude_tui_busy(&pane);
    if ready {
        PaneVerdict::Ready
    } else {
        PaneVerdict::NotReady
    }
}

fn judge_codex(capture: &str) -> PaneVerdict {
    let (composer_marker_detected, prompt_draft_detected, pane) =
        prompt_readiness_from_ansi_pane(capture);
    let lines: Vec<&str> = pane.lines().collect();
    let tail = lines[lines.len().saturating_sub(CODEX_MODAL_TAIL_LINES)..].join("\n");
    let raw = PromptReadinessSnapshot {
        composer_marker_detected,
        prompt_draft_detected,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: tail,
    };
    if steering_snapshot_decision(&raw).is_err() {
        return PaneVerdict::Modal;
    }
    if composer_marker_detected && !prompt_draft_detected {
        PaneVerdict::Ready
    } else {
        PaneVerdict::NotReady
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
        let Some(draft) = active_composer_visible_prompt_draft_in_pane(&plain) else {
            return false;
        };
        return draft == frame
            && !plain.contains("[Pasted Content ")
            && !plain.to_ascii_lowercase().contains("approval required");
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
    claude_composer_rows(&plain)
        .is_some_and(|shown| shown.into_iter().eq(rows.iter().map(String::as_str)))
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
    let start = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with('❯'))?;
    let end = (start + 1..lines.len()).find(|&i| {
        let line = lines[i].trim();
        line.chars().all(|c| c == '─') && line.len() >= 3
    })?;
    let first = lines[start].trim_start().strip_prefix('❯')?;
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
