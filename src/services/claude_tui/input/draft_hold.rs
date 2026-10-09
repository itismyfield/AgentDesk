//! Automatic writes on a pane that may be protecting a person's draft: each one asks the
//! composer admission first and sends no key when the pane is held.

use super::{
    CancelToken, PROMPT_READY_TIMEOUT_ERROR_PREFIX, PromptReadinessKind, TuiInputAction,
    host_input, prompt_readiness_snapshot, run_actions,
};
use crate::services::claude_tui::busy_inject::submit_sighting;
use crate::services::claude_tui::composer_lock::{
    DraftGuard, DraftSighting, admit_composer_write, guard_draft, with_composer_mutation_lock,
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
    let capture = || host_input::observe_draft(session_name);
    admit_composer_write(session_name, capture)
        .map_err(|_| held_error(readiness, "draft_recovery_hold"))
}

fn held_error(readiness: PromptReadinessKind, reason: &str) -> String {
    format!(
        "{PROMPT_READY_TIMEOUT_ERROR_PREFIX} {} prompt input readiness held; reason={reason}; previous_tui_turn_still_running=false; prompt_marker_detected=true",
        readiness.label()
    )
}

/// Reads the composer with attributes just before a follow-up types: a person's draft protects the
/// pane until it is gone, an unreadable composer holds only this input; the turn bridge requeues both.
pub(super) fn refuse_composer_draft(
    session_name: &str,
    readiness: PromptReadinessKind,
) -> Result<(), String> {
    let capture = host_input::observe_draft(session_name);
    let sighting = capture.as_deref().map(submit_sighting);
    let reason = match sighting {
        Some(DraftSighting::Settled) => return Ok(()),
        Some(DraftSighting::PersonDraft) => {
            guard_draft(session_name, DraftGuard::DraftRestored);
            "draft_recovery_hold"
        }
        _ => "composer_unread",
    };
    tracing::info!(
        tmux_session_name = session_name,
        readiness = readiness.label(),
        reason,
        capture_available = capture.is_some(),
        "claude_tui follow-up held: the composer shows a person's draft or could not be read"
    );
    Err(held_error(readiness, reason))
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
