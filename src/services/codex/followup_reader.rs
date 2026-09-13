//! Follow-up reader completion and recovery for the existing tmux wrapper.
use super::*;

pub(super) fn send_followup_to_tmux(
    prompt: &str,
    output_path: &str,
    input_fifo_path: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<FollowupResult, String> {
    let start_offset = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    if let Err(error) = send_codex_pipe_prompt_to_fifo(input_fifo_path, prompt) {
        // The reuse gate passed, but the FIFO/reader can disappear between that
        // check and this write (wrapper exited, cleaned up its FIFO, broken
        // pipe). Treat those infrastructure failures as a stale session and
        // request recreation (which resumes via session id) instead of surfacing
        // a hard provider error — mirroring the Claude/Qwen FIFO follow-up path.
        if should_recreate_session_after_followup_fifo_error(&error) {
            return Ok(FollowupResult::RecreateSession { error });
        }
        return Err(format!(
            "Failed to send Codex follow-up prompt to input FIFO: {error}"
        ));
    }

    if let Some(ref token) = cancel_token {
        token.bind_unmanaged_session_name(tmux_session_name);
    }

    let read_result = match read_output_file_until_result_tracked(
        output_path,
        start_offset,
        sender.clone(),
        cancel_token,
        SessionProbe::tmux_with_structured_output(
            tmux_session_name.to_string(),
            ProviderKind::Codex,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper),
            output_path.to_string(),
        ),
    ) {
        Ok(read_result) => read_result,
        Err(failure) => {
            return failure.recover_followup(|failure| {
                let output_exists = std::fs::metadata(output_path).is_ok();
                let current_file_len = std::fs::metadata(output_path).ok().map(|meta| meta.len());
                // Codex legacy wrapper runs in tmux fifo mode: follow-ups are
                // written to a named input FIFO, so the FIFO file is the input
                // transport and can be stat'd directly.
                let input_exists = std::fs::metadata(input_fifo_path).is_ok();
                let session_alive = tmux_session_has_live_pane(tmux_session_name);
                let ready_for_input = session_alive
                    && crate::services::tui_turn_state::jsonl_ready_for_input(
                        &ProviderKind::Codex,
                        Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui),
                        std::path::Path::new(output_path),
                        Some(failure.last_offset),
                    )
                    .is_some_and(crate::services::tui_turn_state::TuiReadyState::is_ready);

                if let Some(fallback) = tmux_followup_fallback_after_read_error(
                    start_offset,
                    failure.last_offset,
                    current_file_len,
                    session_alive,
                    ready_for_input,
                    output_exists,
                    input_exists,
                ) {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ codex follow-up read failed for {tmux_session_name}: {}; attaching fallback watcher at offset {} (ready_for_input={}, emit_done={})",
                        failure.error,
                        fallback.last_offset,
                        ready_for_input,
                        fallback.emit_synthetic_done
                    );
                    if fallback.emit_synthetic_done {
                        let _ = sender.send(StreamMessage::Done {
                            result: String::new(),
                            session_id: None,
                    });
                }
                let _ = sender.send(StreamMessage::TmuxReady {
                    output_path: output_path.to_string(),
                    input_fifo_path: input_fifo_path.to_string(),
                    tmux_session_name: tmux_session_name.to_string(),
                    last_offset: fallback.last_offset,
                });
                return Ok(FollowupResult::Delivered);
            }

            if !session_alive {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ codex follow-up read failed and tmux session died for {tmux_session_name}: {}; recreating session",
                    failure.error
                );
                return Ok(FollowupResult::RecreateSession {
                    error: failure.error,
                });
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::error!(
                "  [{ts}] ✗ codex follow-up read failed with no watcher fallback for {tmux_session_name}: {} (output_exists={}, input_exists={})",
                failure.error,
                output_exists,
                input_exists
            );
            return Err(failure.error);
            });
        }
    };

    Ok(fold_read_output_result(
        read_result,
        |offset| {
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path: output_path.to_string(),
                input_fifo_path: input_fifo_path.to_string(),
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
            FollowupResult::Delivered
        },
        |_| FollowupResult::RecreateSession {
            error: "session died during follow-up output reading".to_string(),
        },
    ))
}
