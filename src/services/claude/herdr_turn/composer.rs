//! A person's draft and every unreadable or busy composer stay untouched.
use crate::services::claude_tui::busy_inject::{ComposerOwner, composer_owner};
use crate::services::claude_tui::host_input::{InputRefusal, InputRun, run_herdr};
use crate::services::claude_tui::input::{
    PromptReadinessSnapshot, TuiInputAction, final_prompt_ready,
};
use crate::services::provider::{CancelToken, cancel_requested};
use crate::services::session_host::HerdrTarget;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

const COMPOSER_READY_WAIT: Duration = Duration::from_secs(10);
const COMPOSER_POLL: Duration = Duration::from_millis(200);

#[derive(Clone, Copy)]
pub(crate) enum WaitStop {
    Cancelled,
    TimedOut,
}

/// Only the provider classifies a screen; cancellation and the polling budget are shared.
pub(crate) fn wait_for<T>(
    cancel: Option<&CancelToken>,
    timeout: Duration,
    mut observe: impl FnMut() -> ControlFlow<T>,
) -> Result<T, WaitStop> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancel_requested(cancel) {
            return Err(WaitStop::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(WaitStop::TimedOut);
        }
        if let ControlFlow::Break(result) = observe() {
            return Ok(result);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(WaitStop::TimedOut);
        }
        if cancel_requested(cancel) {
            return Err(WaitStop::Cancelled);
        }
        std::thread::sleep(remaining.min(COMPOSER_POLL));
    }
}

fn observation(capture: Option<&str>) -> ControlFlow<bool> {
    let Some(pane) = capture else {
        return ControlFlow::Continue(());
    };
    let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(pane);
    let plain =
        crate::services::claude_tui::prompt_readiness::normalize_prompt_readiness_panel_in_capture(
            &plain,
        );
    let owner = composer_owner(pane);
    if matches!(owner, ComposerOwner::Person | ComposerOwner::AgentDesk)
        || crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal(&plain)
    {
        return ControlFlow::Break(false);
    }
    let snapshot = PromptReadinessSnapshot {
        prompt_marker_detected:
            crate::services::tmux_common::tmux_capture_indicates_claude_tui_ready_for_input(&plain),
        prompt_draft_detected: owner != ComposerOwner::Empty,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: plain.clone(),
    };
    if final_prompt_ready(&snapshot) {
        ControlFlow::Break(true)
    } else {
        ControlFlow::Continue(())
    }
}

/// The capture precedes the gated paste; it never clears a draft or confirms a dialog.
pub(super) fn run(
    target: &HerdrTarget,
    plan: &[TuiInputAction],
    cancel: Option<&CancelToken>,
) -> InputRun {
    match wait_for(cancel, COMPOSER_READY_WAIT, || {
        observation(target.capture_ansi(-80).as_deref())
    }) {
        Ok(true) => run_herdr(target, plan, cancel),
        Err(WaitStop::Cancelled) => InputRun::Cancelled { confirmed: 0 },
        Ok(false) | Err(WaitStop::TimedOut) => InputRun::Refused(InputRefusal::Unknown),
    }
}

#[cfg(test)]
#[path = "composer_tests.rs"]
mod composer_tests;
