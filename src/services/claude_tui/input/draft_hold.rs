//! Automatic writes on a pane that may be protecting a person's draft: each one asks the
//! composer admission first and sends no key when the pane is held.

use super::{
    CancelToken, PROMPT_READY_CAPTURE_SCROLLBACK, PROMPT_READY_TIMEOUT_ERROR_PREFIX,
    PromptReadinessKind, TuiInputAction, host_input, prompt_readiness_snapshot, run_actions,
};
use crate::services::claude_tui::composer_lock::{
    admit_composer_write, with_composer_mutation_lock,
};
use crate::services::claude_tui::startup_dialog::{
    ClaudeStartupDialog, detect_claude_startup_dialog,
};

/// A pane held for draft recovery refuses the write before any key, with an error in the
/// follow-up readiness family that the turn bridge requeues while keeping the session.
pub(super) fn admit_automatic_write(
    session_name: &str,
    readiness: PromptReadinessKind,
) -> Result<(), String> {
    let capture = || host_input::observe_legacy(session_name, PROMPT_READY_CAPTURE_SCROLLBACK).0;
    admit_composer_write(session_name, capture).map_err(|_| {
        format!(
            "{PROMPT_READY_TIMEOUT_ERROR_PREFIX} {} prompt input readiness held; reason=draft_recovery_hold; previous_tui_turn_still_running=false; prompt_marker_detected=true",
            readiness.label()
        )
    })
}

/// One Enter under the composer lock, only while the same dialog shows and the pane takes
/// automatic writes; a pane protecting a person's draft gets no key.
pub(super) fn dismiss_startup_dialog(
    session_name: &str,
    readiness: PromptReadinessKind,
    dialog: &ClaudeStartupDialog,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    with_composer_mutation_lock(session_name, || {
        if admit_automatic_write(session_name, readiness).is_err() {
            return Ok(());
        }
        let shown = prompt_readiness_snapshot(session_name).pane_tail;
        if detect_claude_startup_dialog(&shown).as_ref() != Some(dialog) {
            return Ok(());
        }
        run_actions(session_name, &[TuiInputAction::Enter], cancel_token)
    })
}
