//! First prompt receipt and response observation have separate lifetimes.
use super::*;

pub(crate) struct Submitted {
    start_offset: u64,
    hook_rx: tokio::sync::broadcast::Receiver<crate::services::claude_tui::hook_server::HookEvent>,
    hook_events_after: chrono::DateTime<chrono::Utc>,
}

pub(super) fn submit(
    transcript_path_string: &str,
    fresh_turn_start_offset: u64,
    cancel_token: Option<&CancelToken>,
    tmux_session_name: &str,
    prompt: &str,
) -> Result<Submitted, SubmissionError> {
    submit_using(
        transcript_path_string,
        fresh_turn_start_offset,
        || {
            crate::services::claude_tui::input::send_prompt_with_readiness(
                tmux_session_name,
                prompt,
                crate::services::claude_tui::input::PromptReadinessKind::FreshTurn,
                cancel_token,
            )
        },
        std::thread::sleep,
        tmux_session_name,
    )
}

/// The first prompt of an execution the caller just created, for the input owner's effect; the
/// response stays with the provider's observer, which takes the returned ticket.
#[cfg(all(test, unix))]
pub(crate) fn submit_created(
    transcript_path_string: &str,
    fresh_turn_start_offset: u64,
    tmux_session_name: &str,
    prompt: &str,
) -> Result<Submitted, SubmissionError> {
    submit(
        transcript_path_string,
        fresh_turn_start_offset,
        None,
        tmux_session_name,
        prompt,
    )
}

/// The created execution's first prompt through the owner waiting for it: the owner reserves its
/// frame on the target read from the created pane, and the observer keeps the submit's ticket.
pub(super) fn submit_owned(
    hook: crate::services::claude_tui::owner_input::LaunchHook,
    transcript_path_string: &str,
    fresh_turn_start_offset: u64,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    resolved_session_id: &str,
) -> Result<Submitted, SubmissionError> {
    use crate::services::claude_tui::owner_input;
    let target = owner_input::created_tmux_target("claude", tmux_session_name)?;
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let (ticket, transcript, session) = (
        slot.clone(),
        transcript_path_string.to_owned(),
        tmux_session_name.to_owned(),
    );
    let submit = Box::new(move |frame: String| {
        let submitted = submit(
            &transcript,
            fresh_turn_start_offset,
            cancel_token.as_deref(),
            &session,
            &frame,
        )?;
        *ticket.lock().unwrap_or_else(|e| e.into_inner()) = Some(submitted);
        Ok(())
    });
    let source = serde_json::json!({"kind":"pre_source",
        "expected_session_id":resolved_session_id,
        "expected_transcript":transcript_path_string,"binding_watermark":null});
    hook(target, source, submit)?;
    let submitted = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    submitted.ok_or_else(|| {
        "owner submitted without the first prompt's ticket"
            .to_string()
            .into()
    })
}

fn submit_using(
    transcript_path_string: &str,
    fresh_turn_start_offset: u64,
    mut ready: impl FnMut() -> Result<(), SubmissionError>,
    mut sleep: impl FnMut(std::time::Duration),
    tmux_session_name: &str,
) -> Result<Submitted, SubmissionError> {
    for attempt in 1..=CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS {
        let hook_rx = crate::services::claude_tui::hook_server::subscribe_hook_events();
        let turn_started_at = chrono::Utc::now();
        match ready() {
            Ok(()) => {
                let hook_events_after = chrono::Utc::now();
                let start_offset = claude_tui_turn_start_offset_after_timestamp(
                    std::path::Path::new(transcript_path_string),
                    turn_started_at,
                    fresh_turn_start_offset,
                );
                return Ok(Submitted {
                    start_offset,
                    hook_rx,
                    hook_events_after,
                });
            }
            Err(SubmissionError::Guarded(run)) => return Err(SubmissionError::Guarded(run)),
            Err(hold @ SubmissionError::Held(_)) => return Err(hold),
            Err(SubmissionError::Legacy(error))
                if should_retry_claude_tui_fresh_prompt_ready(&error, attempt) =>
            {
                let backoff = claude_tui_fresh_prompt_ready_backoff(attempt);
                debug_log(&format!(
                    "Claude TUI fresh prompt readiness timed out on attempt {}/{}; retrying after {}s",
                    attempt,
                    CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS,
                    backoff.as_secs()
                ));
                tracing::warn!(
                    tmux_session_name = %tmux_session_name,
                    attempt = attempt,
                    max_attempts = CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS,
                    backoff_secs = backoff.as_secs(),
                    error = %error,
                    "claude_tui fresh prompt readiness retry scheduled"
                );
                sleep(backoff);
            }
            Err(SubmissionError::Legacy(error)) => {
                if crate::services::claude_tui::input::is_prompt_ready_timeout_error(&error) {
                    return Err(exhausted(Some(&error)).into());
                }
                return Err(error.into());
            }
        }
    }

    Err(exhausted(None).into())
}

fn exhausted(detail: Option<&str>) -> String {
    match detail {
        Some(error) => format!(
            "{error}; fresh prompt readiness attempts exhausted ({} attempts)",
            CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS
        ),
        None => format!(
            "claude tui fresh prompt readiness attempts exhausted ({} attempts)",
            CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS
        ),
    }
}

pub(super) fn observe(
    transcript_path_string: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    resolved_session_id: &str,
    submitted: Submitted,
) -> Result<(ReadOutputResult, ReadHarvestStats, u64), SubmissionError> {
    let Submitted {
        start_offset,
        hook_rx,
        hook_events_after,
    } = submitted;
    read_claude_tui_transcript_until_done(
        transcript_path_string,
        start_offset,
        sender.clone(),
        cancel_token.clone(),
        tmux_session_name,
        resolved_session_id,
        hook_rx,
        hook_events_after,
    )
    .map(|(result, harvest)| (result, harvest, start_offset))
    .map_err(SubmissionError::from)
}

#[cfg(test)]
mod tests;
