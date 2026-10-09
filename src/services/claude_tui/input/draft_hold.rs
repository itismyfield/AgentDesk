//! Automatic writes on a pane that may be protecting a person's draft: each one asks the
//! composer admission first and sends no key when the pane is held.

use super::{
    CancelToken, PROMPT_READY_TIMEOUT_ERROR_PREFIX, PromptReadinessKind, TuiInputAction,
    host_input, prompt_readiness_snapshot, run_actions,
};
use crate::services::claude_tui::busy_inject::{ComposerOwner, composer_owner};
use crate::services::claude_tui::composer_lock::{
    DraftGuard, admit_composer_write, guard_draft, with_composer_mutation_lock,
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

/// Why an automatic write must not type into the composer `capture` shows, if it must not; a
/// person's draft also protects the pane until a later capture sees it gone.
pub(crate) fn composer_refusal(session_name: &str, capture: Option<&str>) -> Option<&'static str> {
    match capture.map(composer_owner) {
        Some(ComposerOwner::Empty) => None,
        Some(ComposerOwner::Person) => {
            guard_draft(session_name, DraftGuard::DraftRestored);
            Some("draft_recovery_hold")
        }
        Some(ComposerOwner::AgentDesk) => Some("agentdesk_prompt_in_composer"),
        Some(ComposerOwner::Unread) | None => Some("composer_unread"),
    }
}

/// The requeued refusal for the composer `capture` shows, logged.
fn held(session_name: &str, readiness: PromptReadinessKind, capture: Option<&str>) -> String {
    let reason = composer_refusal(session_name, capture).unwrap_or("composer_unread");
    tracing::info!(
        tmux_session_name = session_name,
        readiness = readiness.label(),
        reason,
        capture_available = capture.is_some(),
        "claude_tui follow-up held: the composer is not empty or could not be read"
    );
    held_error(readiness, reason)
}

/// Reads the composer with attributes just before a follow-up types; anything but an empty composer
/// refuses with an error the turn bridge requeues.
pub(super) fn refuse_composer_draft(
    session_name: &str,
    readiness: PromptReadinessKind,
) -> Result<(), String> {
    let capture = host_input::observe_draft(session_name);
    if capture.as_deref().map(composer_owner) == Some(ComposerOwner::Empty) {
        return Ok(());
    }
    Err(held(session_name, readiness, capture.as_deref()))
}

/// Before clearing a draft the pane text called stranded: only a prompt AgentDesk typed may be
/// cleared (`Ok(true)`), an empty composer needs nothing, anything else refuses as a submit would.
pub(crate) fn stranded_draft_is_ours(
    session_name: &str,
    readiness: PromptReadinessKind,
) -> Result<bool, String> {
    let capture = host_input::observe_draft(session_name);
    match capture.as_deref().map(composer_owner) {
        Some(ComposerOwner::Empty) => Ok(false),
        Some(ComposerOwner::AgentDesk) => Ok(true),
        _ => Err(held(session_name, readiness, capture.as_deref())),
    }
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
