//! First prompt receipt and response observation have separate lifetimes.
use super::*;

pub(super) struct Submitted {
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
    for attempt in 1..=CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS {
        let hook_rx = crate::services::claude_tui::hook_server::subscribe_hook_events();
        let turn_started_at = chrono::Utc::now();
        match crate::services::claude_tui::input::send_prompt_with_readiness(
            tmux_session_name,
            prompt,
            crate::services::claude_tui::input::PromptReadinessKind::FreshTurn,
            cancel_token,
        ) {
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
                std::thread::sleep(backoff);
            }
            Err(SubmissionError::Legacy(error)) => {
                if crate::services::claude_tui::input::is_prompt_ready_timeout_error(&error) {
                    return Err(format!(
                        "{}; fresh prompt readiness attempts exhausted ({} attempts)",
                        error, CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS
                    )
                    .into());
                }
                return Err(error.into());
            }
        }
    }

    Err(format!(
        "claude tui fresh prompt readiness attempts exhausted ({} attempts)",
        CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS
    )
    .into())
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
