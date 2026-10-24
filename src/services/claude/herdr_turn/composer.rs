//! A person's draft and every unreadable or busy composer stay untouched.
use crate::services::claude_tui::busy_inject::{ComposerOwner, composer_owner};
use crate::services::claude_tui::host_input::{InputRefusal, InputRun, run_herdr};
use crate::services::claude_tui::input::{
    PromptReadinessSnapshot, TuiInputAction, final_prompt_ready,
};
use crate::services::provider::CancelToken;
use crate::services::session_host::HerdrTarget;

fn ready(capture: Option<&str>) -> bool {
    let Some(pane) = capture else { return false };
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(pane);
    let plain =
        crate::services::claude_tui::prompt_readiness::normalize_prompt_readiness_panel_in_capture(
            &plain,
        );
    let snapshot = PromptReadinessSnapshot {
        prompt_marker_detected:
            crate::services::tmux_common::tmux_capture_indicates_claude_tui_ready_for_input(&plain),
        prompt_draft_detected: composer_owner(pane) != ComposerOwner::Empty,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: plain.clone(),
    };
    final_prompt_ready(&snapshot)
        && !crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal(
            &plain,
        )
}

/// The capture precedes the gated paste; it never clears a draft or confirms a dialog.
pub(super) fn run(
    target: &HerdrTarget,
    plan: &[TuiInputAction],
    cancel: Option<&CancelToken>,
) -> InputRun {
    if !ready(target.capture_ansi(-80).as_deref()) {
        return InputRun::Refused(InputRefusal::Unknown);
    }
    run_herdr(target, plan, cancel)
}

#[cfg(test)]
#[path = "composer_tests.rs"]
mod composer_tests;
