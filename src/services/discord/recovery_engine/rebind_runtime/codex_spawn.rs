use super::*;

pub(in crate::services::discord::recovery_engine) fn spawn_codex_tui_rebind_relay_output(
    tmux_session_name: &str,
    rollout_path: &str,
    raw_start_offset: u64,
    truncate_relay_output: bool,
    watcher_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    session_id: Option<String>,
    already_relayed_response: String,
    already_normalized_replay_events: Vec<serde_json::Value>,
) -> Result<String, RebindError> {
    let relay_output_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let (relay_generation_gate, relay_generation) =
        crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
            use crate::services::tui_prompt_dedupe as dedupe;
            let verified = dedupe::codex_verified_requires_proof_under_source_authority(authority);
            if !dedupe::codex_verified_source_allowed_under_source_authority(
                authority,
                rollout_path,
                session_id.as_deref(),
            ) || (truncate_relay_output && verified)
            {
                return Err(RebindError::Internal(
                    "Codex rebind requires source proof, permission and retained relay checkpoint"
                        .into(),
                ));
            }
            let generation =
                prepare_codex_rebind_relay_generation(&relay_output_path, truncate_relay_output)?;
            let installed = crate::services::codex_tui::session::install_codex_tui_runtime_binding_under_source_authority(
                authority,
                Some(raw_start_offset),
                dedupe::TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::CodexTui,
                    output_path: rollout_path.to_string(),
                    relay_output_path: Some(relay_output_path.clone()),
                    input_fifo_path: None,
                    session_id: session_id.clone(),
                    last_offset: raw_start_offset,
                    relay_last_offset: Some(0),
                },
            );
            if (verified && !installed)
                || !dedupe::codex_verified_source_allowed_under_source_authority(
                    authority,
                    rollout_path,
                    session_id.as_deref(),
                )
            {
                return Err(RebindError::Internal(
                    "Codex rebind publication was rejected".into(),
                ));
            }
            Ok(generation)
        })?;

    let tmux_session_name = tmux_session_name.to_string();
    let rollout_path = std::path::PathBuf::from(rollout_path);
    let relay_path = std::path::PathBuf::from(&relay_output_path);
    let watcher_cancel_for_writer = watcher_cancel.clone();
    #[cfg(test)]
    REBIND_WRITER_SPAWNS.with(|count| count.set(count.get() + 1));
    std::thread::Builder::new()
        .name("codex_tui_rebind_relay_writer".to_string())
        .spawn(move || {
            let (sender, receiver) =
                std::sync::mpsc::channel::<crate::services::agent_protocol::StreamMessage>();
            let tail_rollout_path = rollout_path.clone();
            let tail_tmux_session_name = tmux_session_name.clone();
            let tail_session_id = session_id.clone();
            let tail_cancel_token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
            let cancel_bridge_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let watcher_cancel_for_bridge = watcher_cancel_for_writer.clone();
            let tail_cancel_for_bridge = tail_cancel_token.clone();
            let cancel_bridge_done_for_thread = cancel_bridge_done.clone();
            let cancel_bridge_handle = std::thread::Builder::new()
                .name("codex_tui_rebind_cancel_bridge".to_string())
                .spawn(move || {
                    while !cancel_bridge_done_for_thread
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        if watcher_cancel_for_bridge.load(std::sync::atomic::Ordering::Relaxed) {
                            tail_cancel_for_bridge
                                .cancelled
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                });
            if let Err(error) = &cancel_bridge_handle {
                tracing::warn!(
                    tmux_session = %tmux_session_name,
                    error = %error,
                    "failed to spawn Codex TUI rebind cancel bridge"
                );
            }
            let watcher_cancel_for_alive = watcher_cancel_for_writer.clone();
            let tail_handle = std::thread::Builder::new()
                .name("codex_tui_rebind_rollout_tail".to_string())
                .spawn(move || {
                    crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset_for_tmux(
                        &tail_rollout_path,
                        raw_start_offset,
                        tail_session_id.as_deref(),
                        sender,
                        Some(tail_cancel_token),
                        || {
                            !watcher_cancel_for_alive
                                .load(std::sync::atomic::Ordering::Relaxed)
                                && crate::services::tui_prompt_dedupe::codex_verified_source_allowed(
                                    &tail_tmux_session_name, &tail_rollout_path.to_string_lossy(), tail_session_id.as_deref())
                                && not_dead(observe_liveness(&tail_tmux_session_name, None))
                        },
                        &tail_tmux_session_name,
                    )
                });

            let writer_result = write_codex_rebind_normalized_stream_for_generation(
                &relay_path,
                receiver,
                already_relayed_response,
                already_normalized_replay_events,
                &relay_generation_gate,
                relay_generation,
                Some((&tmux_session_name, &rollout_path, session_id.as_deref())),
            );
            if let Err(error) = &writer_result {
                tracing::warn!(
                    tmux_session = %tmux_session_name,
                    relay_output_path = %relay_path.display(),
                    error = %error,
                    "Codex TUI rebind relay writer failed"
                );
            }

            match tail_handle {
                Ok(handle) => match handle.join() {
                    Ok(Ok(read_result)) => {
                        let (final_offset, advance_cursor) = match read_result {
                            crate::services::provider::ReadOutputResult::Completed { offset }
                            | crate::services::provider::ReadOutputResult::SessionDied {
                                offset,
                            } => (offset, true),
                            crate::services::provider::ReadOutputResult::Cancelled { offset } => {
                                (offset, false)
                            }
                        };
                        if writer_result.is_ok() && advance_cursor {
                            crate::services::codex_tui::session::advance_codex_tui_runtime_binding_and_marker_offset(
                                &tmux_session_name,
                                &rollout_path,
                                final_offset,
                            );
                        } else if !advance_cursor {
                            tracing::warn!(
                                tmux_session = %tmux_session_name,
                                rollout_path = %rollout_path.display(),
                                final_offset,
                                "Codex TUI rebind relay was cancelled with watcher; preserving previous raw rollout cursor for retry"
                            );
                        } else {
                            tracing::warn!(
                                tmux_session = %tmux_session_name,
                                rollout_path = %rollout_path.display(),
                                final_offset,
                                "Codex TUI rebind relay writer failed; preserving previous raw rollout cursor for retry"
                            );
                        }
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(
                            tmux_session = %tmux_session_name,
                            rollout_path = %rollout_path.display(),
                            error = %error,
                            "Codex TUI rebind rollout tail failed"
                        );
                    }
                    Err(_) => {
                        tracing::warn!(
                            tmux_session = %tmux_session_name,
                            rollout_path = %rollout_path.display(),
                            "Codex TUI rebind rollout tail panicked"
                        );
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        tmux_session = %tmux_session_name,
                        rollout_path = %rollout_path.display(),
                        error = %error,
                        "failed to spawn Codex TUI rebind rollout tail"
                    );
                }
            }
            cancel_bridge_done.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Ok(handle) = cancel_bridge_handle {
                let _ = handle.join();
            }
        })
        .map_err(|error| {
            RebindError::Internal(format!("spawn Codex TUI rebind relay writer: {error}"))
        })?;

    Ok(relay_output_path)
}
