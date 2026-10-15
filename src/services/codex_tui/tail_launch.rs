//! Cold Direct TUI tail start: resume metadata is checked before the spawn marker exists, and a
//! verified resume records the source it executed before the tail reads that source.

use crate::services::agent_protocol::StreamMessage;
use crate::services::codex_tui::rollout_tail::{self, CodexTuiTailResult};
use crate::services::codex_tui::session::CodexTuiSessionSelection;
use crate::services::provider::CancelToken;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc::Sender};
use std::time::SystemTime;

#[cfg(test)]
type SelectionEdit = Box<dyn FnOnce(&mut CodexTuiSessionSelection)>;
#[cfg(test)]
type ProofBoundary = Box<dyn FnOnce(&BindingContext)>;
#[cfg(test)]
thread_local! {
    pub(crate) static SELECTION_FOR_TESTS: std::cell::RefCell<Option<SelectionEdit>> = Default::default();
    pub(crate) static BEFORE_RESUME_PROOF: std::cell::RefCell<Option<ProofBoundary>> = Default::default();
}

/// A resume carries `(rollout_path, start_offset, selected_session_id)`; `None` tails fresh.
pub(crate) struct ValidatedTail {
    resume: Option<(PathBuf, u64, String)>,
}

fn verified(context: &BindingContext) -> bool {
    context.source_policy.as_deref() == Some("verified")
}

/// Deterministic selection checks; the marker written after this can prove they passed.
pub(crate) fn validate_selection(
    context: &BindingContext,
    selection: &CodexTuiSessionSelection,
) -> Result<ValidatedTail, String> {
    #[cfg(test)]
    let selection = &SELECTION_FOR_TESTS.with_borrow_mut(|slot| {
        let mut selection = selection.clone();
        if let Some(edit) = slot.take() {
            edit(&mut selection);
        }
        selection
    });
    let resume = if selection.resume {
        let rollout_path = selection
            .rollout_path
            .clone()
            .ok_or_else(|| "Codex TUI resume selected without rollout path".to_string())?;
        let start_offset = selection.rollout_start_offset.unwrap_or(0);
        let selected_session_id = selection
            .selected_session_id
            .clone()
            .ok_or_else(|| "Codex TUI resume selected without session id".to_string())?;
        Some((rollout_path, start_offset, selected_session_id))
    } else {
        None
    };
    let executed = resume.as_ref().map(|(_, _, id)| id.as_str());
    if verified(context)
        && (context.launch_mode
            != if executed.is_some() {
                "resume"
            } else {
                "fresh"
            }
            || context.expected_native_session_id.as_deref() != executed)
    {
        return Err("Codex TUI resume selection does not match its verified launch".to_string());
    }
    Ok(ValidatedTail { resume })
}

/// A verified resume must own its executed source before the tail starts; failure keeps the pane.
pub(crate) fn record_resume_proof(
    context: &BindingContext,
    tail: &ValidatedTail,
) -> Result<(), String> {
    let Some((rollout_path, start_offset, selected_session_id)) =
        tail.resume.as_ref().filter(|_| verified(context))
    else {
        return Ok(());
    };
    #[cfg(test)]
    BEFORE_RESUME_PROOF.with_borrow_mut(|slot| {
        if let Some(boundary) = slot.take() {
            boundary(context);
        }
    });
    match crate::services::tui_prompt_dedupe::codex_verified::record_executed_resume(
        context,
        selected_session_id,
        rollout_path,
        *start_offset,
    ) {
        Ok(true) => Ok(()),
        Ok(false) => Err("Codex TUI resume proof is still pending".to_string()),
        Err(error) => Err(format!("Codex TUI resume proof unavailable: {error}")),
    }
}

/// A resume tails the selected rollout from its committed offset; otherwise the latest cwd rollout.
pub(crate) fn dispatch(
    tail: ValidatedTail,
    working_dir: &str,
    rollout_modified_since: SystemTime,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    tmux_session_name: &str,
    prompt: &str,
) -> Result<CodexTuiTailResult, String> {
    let liveness =
        || crate::services::tmux_diagnostics::tmux_session_pane_liveness(tmux_session_name);
    match tail.resume {
        Some((rollout_path, start_offset, selected_session_id)) => {
            rollout_tail::tail_resumed_rollout_for_session_with_handoff_for_tmux(
                Path::new(working_dir),
                &selected_session_id,
                &rollout_path,
                start_offset,
                rollout_modified_since,
                sender,
                cancel_token,
                liveness,
                tmux_session_name,
                Some(prompt),
            )
        }
        None => rollout_tail::tail_latest_rollout_for_cwd_with_handoff_for_tmux(
            Path::new(working_dir),
            rollout_modified_since,
            sender,
            cancel_token,
            liveness,
            tmux_session_name,
            Some(prompt),
        ),
    }
}
