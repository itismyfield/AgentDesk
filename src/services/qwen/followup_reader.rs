//! Follow-up reader completion and recovery for the existing tmux wrapper.
use super::*;

pub(super) fn send_followup_to_tmux(
    prompt: &str,
    output_path: &str,
    input_fifo_path: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<FollowupResult, String> {
    let start_offset = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    let write_result = std::fs::OpenOptions::new()
        .write(true)
        .open(input_fifo_path)
        .map_err(|e| format!("Failed to open input FIFO: {}", e))
        .and_then(|mut fifo| {
            let encoded = format!(
                "{}{}",
                TMUX_PROMPT_B64_PREFIX,
                BASE64_STANDARD.encode(prompt.as_bytes())
            );
            writeln!(fifo, "{}", encoded)
                .map_err(|e| format!("Failed to write to input FIFO: {}", e))?;
            fifo.flush()
                .map_err(|e| format!("Failed to flush input FIFO: {}", e))?;
            Ok(())
        });

    if let Err(e) = write_result {
        if should_recreate_session_after_followup_fifo_error(&e) {
            return Ok(FollowupResult::RecreateSession { error: e });
        }
        return Err(e);
    }

    crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
        ProviderKind::Qwen.as_str(),
        tmux_session_name,
        prompt,
    );

    if let Some(ref token) = cancel_token {
        token.bind_unmanaged_session_name(tmux_session_name);
    }

    let read_result = match qwen_read_output_file_until_result_tracked(
        output_path,
        start_offset,
        sender.clone(),
        cancel_token,
        SessionProbe::tmux(tmux_session_name.to_string(), ProviderKind::Qwen),
        Some(tmux_session_name),
    ) {
        Ok(read_result) => read_result,
        Err(failure) => {
            if failure.source_changed {
                crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
                    ProviderKind::Qwen.as_str(),
                    tmux_session_name,
                    prompt,
                );
            }
            return failure.recover_followup(|failure| {
                let output_exists = std::fs::metadata(output_path).is_ok();
                let current_file_len = std::fs::metadata(output_path).ok().map(|meta| meta.len());
                let input_exists = std::path::Path::new(input_fifo_path).exists();
                let session_alive = tmux_session_has_live_pane(tmux_session_name);
                let ready_for_input = session_alive
                    && crate::services::provider::tmux_session_fallback_ready_for_input(
                        tmux_session_name,
                        &ProviderKind::Qwen,
                        None,
                    )
                    .is_some_and(crate::services::pane_readiness::FallbackPaneReadiness::is_ready);

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
                        "  [{ts}] ⚠ qwen follow-up read failed for {tmux_session_name}: {}; attaching fallback watcher at offset {} (ready_for_input={}, emit_done={})",
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
                register_qwen_tmux_runtime_binding(
                    tmux_session_name,
                    output_path,
                    input_fifo_path,
                    fallback.last_offset,
                );
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
                    "  [{ts}] ⚠ qwen follow-up read failed and tmux session died for {tmux_session_name}: {}; recreating session",
                    failure.error
                );
                crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
                    ProviderKind::Qwen.as_str(),
                    tmux_session_name,
                    prompt,
                );
                return Ok(FollowupResult::RecreateSession {
                    error: failure.error,
                });
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::error!(
                "  [{ts}] ✗ qwen follow-up read failed with no watcher fallback for {tmux_session_name}: {} (output_exists={}, input_exists={})",
                failure.error,
                output_exists,
                input_exists
            );
            crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
                ProviderKind::Qwen.as_str(),
                tmux_session_name,
                prompt,
            );
            return Err(failure.error);
            });
        }
    };

    match read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            register_qwen_tmux_runtime_binding(
                tmux_session_name,
                output_path,
                input_fifo_path,
                offset,
            );
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path: output_path.to_string(),
                input_fifo_path: input_fifo_path.to_string(),
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
            Ok(FollowupResult::Delivered)
        }
        ReadOutputResult::SessionDied { .. } => {
            crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
                ProviderKind::Qwen.as_str(),
                tmux_session_name,
                prompt,
            );
            Ok(FollowupResult::RecreateSession {
                error: "session died during follow-up output reading".to_string(),
            })
        }
    }
}
