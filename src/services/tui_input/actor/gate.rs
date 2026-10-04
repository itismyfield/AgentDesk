//! Pane gate: the existing readiness predicates plus a raw modal veto, failing closed.

use crate::services::claude_tui::prompt_readiness::normalize_prompt_readiness_panel_in_capture;
use crate::services::claude_tui::startup_dialog::detect_claude_startup_dialog;
use crate::services::codex_tui::input::{
    PromptReadinessSnapshot, pane_looks_ready_for_codex_prompt, steering_snapshot_decision,
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
    match provider {
        ShadowProvider::Claude => judge_claude(capture),
        ShadowProvider::Codex => judge_codex(capture),
    }
}

fn judge_claude(capture: &str) -> PaneVerdict {
    let pane = normalize_prompt_readiness_panel_in_capture(capture);
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
    let lines: Vec<&str> = capture.lines().collect();
    let tail = lines[lines.len().saturating_sub(CODEX_MODAL_TAIL_LINES)..].join("\n");
    // With composer and draft forced clean, steering refuses only for modal wording.
    let raw = PromptReadinessSnapshot {
        composer_marker_detected: true,
        prompt_draft_detected: false,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: tail,
    };
    if steering_snapshot_decision(&raw).is_err() {
        return PaneVerdict::Modal;
    }
    if pane_looks_ready_for_codex_prompt(capture) {
        PaneVerdict::Ready
    } else {
        PaneVerdict::NotReady
    }
}
