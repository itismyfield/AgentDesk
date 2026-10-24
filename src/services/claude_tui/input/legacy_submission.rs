use super::*;

pub(super) fn run_actions_with_submission_confirmation(
    session_name: &str,
    actions: &[TuiInputAction],
    submitted_prompt: Option<&str>,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    let payload_attempted = actions.iter().any(|a| {
        matches!(
            a,
            TuiInputAction::PasteBuffer(_) | TuiInputAction::Literal(_)
        )
    });
    let run = host_input::run_legacy(session_name, actions, cancel_token);
    if matches!(&run, host_input::InputRun::Cancelled { confirmed: 0 }) {
        return run.into_legacy();
    }
    let result = run.into_legacy().and_then(|()| {
        confirm_prompt_submission_left_editor(session_name, submitted_prompt, cancel_token)
    });
    if payload_attempted && let Err(error) = result {
        return Err(
            SubmissionError::Held(PreparationHold::uncertain(session_name, error)).into_legacy(),
        );
    }
    result
}

pub(super) fn confirm_prompt_submission_left_editor(
    session_name: &str,
    submitted_prompt: Option<&str>,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    let mut attempt = 0usize;
    loop {
        std::thread::sleep(prompt_submit_settle_for_attempt(attempt));
        check_prompt_cancel(cancel_token)?;
        let (pane, alive) =
            host_input::observe_legacy(session_name, PROMPT_READY_CAPTURE_SCROLLBACK);
        let snapshot = prompt_readiness_snapshot_from_capture(pane.as_deref(), alive);
        let prompt_left = submitted_prompt
            .zip(pane.as_deref())
            .is_some_and(|(prompt, pane)| {
                crate::services::claude_tui::prompt_readiness::composer_still_holds_prompt(
                    pane, prompt,
                )
            });
        check_prompt_cancel(cancel_token)?;

        match prompt_submit_confirmation_decision(
            &snapshot,
            prompt_left,
            attempt,
            PROMPT_SUBMIT_CONFIRM_RETRIES,
        ) {
            PromptSubmitConfirmationDecision::Submitted => return Ok(()),
            PromptSubmitConfirmationDecision::FailedSessionDead => {
                return Err("claude tui session died after prompt submit".to_string());
            }
            PromptSubmitConfirmationDecision::FailedCaptureUnavailable => {
                log_prompt_submit_capture_unavailable(session_name, attempt, &snapshot);
                return Err(format!(
                    "claude tui prompt submit confirmation unavailable after {} retries; capture_available=false",
                    PROMPT_SUBMIT_CONFIRM_RETRIES
                ));
            }
            PromptSubmitConfirmationDecision::FailedDraftStuck => {
                log_prompt_submit_left_draft(session_name, &snapshot);
                return Err(format!(
                    "claude tui prompt submit left draft after {} enter retries; prompt_marker_detected={}; prompt_draft_detected={}; capture_available={}",
                    PROMPT_SUBMIT_CONFIRM_RETRIES,
                    snapshot.prompt_marker_detected,
                    snapshot.prompt_draft_detected,
                    snapshot.capture_available
                ));
            }
            PromptSubmitConfirmationDecision::RetrySnapshot => {
                log_prompt_submit_capture_unavailable(session_name, attempt, &snapshot);
                attempt += 1;
            }
            PromptSubmitConfirmationDecision::RetryEnter => {
                return Err("claude tui prompt submit remains unconfirmed; preserving composer without another Enter".into());
            }
        }
    }
}

pub(super) fn prompt_submit_needs_enter_retry(snapshot: &PromptReadinessSnapshot) -> bool {
    snapshot.tmux_pane_alive && snapshot.prompt_marker_detected && snapshot.prompt_draft_detected
}

pub(super) fn prompt_submit_confirmation_decision(
    snapshot: &PromptReadinessSnapshot,
    prompt_left_in_composer: bool,
    attempt: usize,
    max_retries: usize,
) -> PromptSubmitConfirmationDecision {
    if !snapshot.tmux_pane_alive {
        return PromptSubmitConfirmationDecision::FailedSessionDead;
    }
    if !snapshot.capture_available {
        return if attempt >= max_retries {
            PromptSubmitConfirmationDecision::FailedCaptureUnavailable
        } else {
            PromptSubmitConfirmationDecision::RetrySnapshot
        };
    }
    let draft = prompt_submit_needs_enter_retry(snapshot);
    if draft || prompt_left_in_composer {
        // Only a probed draft fails; a prompt-text match just earns retries (a repaint can lag).
        return match (attempt >= max_retries, draft) {
            (false, _) => PromptSubmitConfirmationDecision::RetryEnter,
            (true, true) => PromptSubmitConfirmationDecision::FailedDraftStuck,
            (true, false) => PromptSubmitConfirmationDecision::Submitted,
        };
    }
    PromptSubmitConfirmationDecision::Submitted
}

pub(super) fn prompt_submit_settle_for_attempt(attempt: usize) -> Duration {
    if attempt == 0 {
        PROMPT_SUBMIT_INITIAL_SETTLE
    } else {
        PROMPT_SUBMIT_RETRY_SETTLE
    }
}

pub(super) fn clear_prompt_draft(session_name: &str) {
    let capture = host_input::observe_draft(session_name);
    let snapshot = prompt_readiness_snapshot_from_capture(
        capture.as_deref(),
        host_input::legacy_pane_alive(session_name),
    );
    if capture.as_deref().map(|capture| {
        crate::services::claude_tui::busy_inject::composer_owner_in_session(session_name, capture)
    }) != Some(crate::services::claude_tui::busy_inject::ComposerOwner::AgentDesk)
    {
        guarded_outcome::preparation_held(&PreparationHold::before(
            session_name,
            "cleanup_not_owned",
            "claude tui cleanup held: composer ownership changed".into(),
        ));
        return;
    }
    let actions = prompt_draft_cleanup_actions(&snapshot);
    if let Err(error) = run_actions(session_name, &actions, PROMPT_DRAFT_CLEANUP_CANCEL_TOKEN) {
        tracing::warn!(
            tmux_session_name = session_name,
            error = %error,
            "failed to clear Claude TUI draft after prompt submit retries"
        );
    }
}
