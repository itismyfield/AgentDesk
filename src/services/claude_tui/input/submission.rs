//! Shared submit entry keeps readiness outside the composer permit.
use super::*;

pub(super) fn send(
    session: &str,
    prompt: &str,
    readiness: PromptReadinessKind,
    cancel: Option<&CancelToken>,
    transcript: Option<&std::path::Path>,
) -> Result<(), SubmissionError> {
    guarded_outcome::capture_preparation(|| {
        send_inner(session, prompt, readiness, cancel, transcript)
    })
}

fn send_inner(
    session: &str,
    prompt: &str,
    readiness: PromptReadinessKind,
    cancel: Option<&CancelToken>,
    transcript: Option<&std::path::Path>,
) -> Result<(), SubmissionError> {
    let selected = guarded::capture_request(session)?;
    #[cfg(unix)]
    {
        guarded_outcome::receipt(session, prompt);
        if let Some(result) =
            guarded::idle_stash::submit(session, prompt, cancel, transcript, selected.as_deref())
        {
            return result.map_err(|e| preparation_hold::idle_stash_stopped(session, e));
        }
    }
    if crate::services::claude_tui::composer_lock::draft_guarded(session) {
        crate::services::claude_tui::composer_lock::with_composer_mutation_lock(session, || {
            admit_automatic_write(session, readiness)
        })?;
    }
    let actions = plan_prompt_submit(prompt)?;
    let ready = match transcript {
        Some(path) => wait_for_prompt_ready_or_idle_transcript(session, readiness, cancel, path),
        None => wait_for_prompt_ready(session, readiness, cancel),
    };
    ready.map_err(|error| preparation_hold::readiness_failure(session, error))?;
    guarded_outcome::receipt(session, prompt);
    if let Some(run) = guarded::selected(session, prompt, cancel, selected.clone()) {
        return SubmissionError::from_run(run).map_err(|e| preparation_hold::stopped(session, e));
    }
    crate::services::claude_tui::composer_lock::with_composer_mutation_lock(session, || {
        admit_automatic_write(session, readiness)?;
        let snapshot = prompt_readiness_snapshot(session);
        let ready = transcript.map_or_else(
            || prompt_marker_confirms_prompt_ready(readiness, &snapshot),
            |path| proven_warm_followup_revalidates_prompt_ready(&snapshot, path),
        );
        if !ready {
            return Err(preparation_hold::changed(session));
        }
        refuse_composer_draft(session, readiness)?;
        super::super::submission_fence::before_first_payload()?;
        crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
            "claude", session, prompt,
        );
        guarded_outcome::receipt(session, prompt);
        run_actions_with_submission_confirmation(session, &actions, Some(prompt), cancel)
    })
    .map_err(|error| guarded::readiness_error(session, error, &selected))
}
