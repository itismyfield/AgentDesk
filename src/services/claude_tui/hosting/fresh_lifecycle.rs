use super::*;

#[cfg(test)]
pub(crate) fn fresh_hold_for_tests(
    path: &std::path::Path,
    session: &str,
    owner: &std::path::Path,
    prompt: &str,
) -> Result<(), String> {
    let (sender, _) = std::sync::mpsc::channel();
    run_claude_tui_fresh_turn_and_finalize(
        path,
        path.to_str().unwrap(),
        sender,
        Some(std::sync::Arc::new(CancelToken::new())),
        session,
        Some(&TeardownClearance::Unkeyed),
        "held-sid",
        None,
        prompt,
        owner.to_str().unwrap(),
    )
}

pub(super) struct FreshTurn<'a> {
    pub(super) transcript_path: &'a std::path::Path,
    pub(super) transcript_path_string: &'a str,
    pub(super) sender: Sender<StreamMessage>,
    pub(super) cancel_token: Option<std::sync::Arc<CancelToken>>,
    pub(super) tmux_session_name: &'a str,
    pub(super) teardown: Option<&'a TeardownClearance>,
    pub(super) resolved_session_id: &'a str,
    pub(super) report_channel_id: Option<u64>,
    pub(super) prompt: &'a str,
    pub(super) owner_path: &'a str,
}

/// Fresh holds retain the pane and owner; other failures keep their teardown policy.
pub(super) fn run(turn: FreshTurn<'_>) -> Result<(), String> {
    let FreshTurn {
        transcript_path,
        transcript_path_string,
        sender,
        cancel_token,
        tmux_session_name,
        teardown,
        resolved_session_id,
        report_channel_id,
        prompt,
        owner_path,
    } = turn;
    // Skip any transcript bytes that predate this launch. Resume and fresh
    // turns both need this guard because a reused/colliding session_id can
    // leave stale JSONL on disk even when the launch is intentionally fresh.
    let fresh_turn_start_offset = claude_tui_fresh_turn_start_offset(transcript_path);
    guarded_outcome::fresh_receipt(
        tmux_session_name,
        prompt,
        transcript_path_string,
        resolved_session_id,
    );
    let fresh_turn_result = run_claude_tui_fresh_turn_with_ready_retry(
        transcript_path_string,
        fresh_turn_start_offset,
        sender.clone(),
        cancel_token,
        tmux_session_name,
        resolved_session_id,
        prompt,
    );
    let (read_result, harvest, turn_read_start_offset) = match fresh_turn_result {
        Ok(result) => result,
        Err(hold @ SubmissionError::Held(_)) => return Err(hold.into_legacy()),
        Err(SubmissionError::Guarded(run)) => {
            return Err(SubmissionError::Guarded(run).into_legacy());
        }
        // The prompt never reached the pane: keep the session and its owner marker.
        Err(SubmissionError::Legacy(error))
            if crate::services::claude_tui::submission_fence::refusal().is_some() =>
        {
            return Err(error);
        }
        Err(SubmissionError::Legacy(error)) => {
            let reason = format!("claude tui fresh turn failed: {error}");
            let component = "claude_tui_provider";
            if teardown_tmux(
                teardown,
                tmux_session_name,
                component,
                "fresh_turn_start_failed",
                &reason,
            )
            .is_ok()
            {
                remove_claude_tui_owner_marker_after_teardown(owner_path);
            }
            return Err(error);
        }
    };
    if matches!(read_result, ReadOutputResult::SessionDied { .. }) {
        let reason = "claude tui session died before turn completion";
        let component = "claude_tui_provider";
        if teardown_tmux(
            teardown,
            tmux_session_name,
            component,
            "fresh_session_died",
            reason,
        )
        .is_ok()
        {
            remove_claude_tui_owner_marker_after_teardown(owner_path);
        }
        return Err(reason.to_string());
    }
    emit_claude_tui_watcher_handoff(
        &sender,
        transcript_path_string,
        tmux_session_name,
        transcript_path,
    );
    let transcript_len = std::fs::metadata(transcript_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    if tui_delivered_zero_harvest(&read_result, &harvest) {
        emit_claude_tui_zero_harvest(
            "claude_tui_zero_harvest_turn_delivered",
            report_channel_id,
            tmux_session_name,
            transcript_path_string,
            turn_read_start_offset,
            transcript_len,
        );
    }
    log_producer_exit(
        "tui_turn_delivered",
        Some(resolved_session_id),
        report_channel_id,
        // #3281: real forwarded-message count (was a hardcoded 0).
        usize::try_from(harvest.forwarded_messages).unwrap_or(usize::MAX),
        serde_json::json!({
            "tmux_session_name": tmux_session_name,
            "transcript_path": transcript_path_string,
            "assistant_text_bytes": harvest.assistant_text_bytes,
            "start_offset": turn_read_start_offset,
            "transcript_len": transcript_len,
            "read_result_kind": read_output_result_kind(&read_result),
        }),
    );
    Ok(())
}

/// On success returns the read result, the harvest counters, and the actual
/// turn-read start offset (the timestamp-scan adjusted offset, #3281) so the
/// producer-exit log can report real values instead of a hardcoded `lines=0`.
#[cfg(unix)]
fn run_claude_tui_fresh_turn_with_ready_retry(
    transcript_path_string: &str,
    fresh_turn_start_offset: u64,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    resolved_session_id: &str,
    prompt: &str,
) -> Result<(ReadOutputResult, ReadHarvestStats, u64), SubmissionError> {
    let submitted = super::first_input::submit(
        transcript_path_string,
        fresh_turn_start_offset,
        cancel_token.as_deref(),
        tmux_session_name,
        prompt,
    )?;
    super::first_input::observe(
        transcript_path_string,
        sender,
        cancel_token,
        tmux_session_name,
        resolved_session_id,
        submitted,
    )
}
