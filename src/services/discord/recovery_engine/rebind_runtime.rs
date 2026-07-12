use super::*;

#[path = "rebind_runtime/codex_relay_generation.rs"]
mod codex_relay_generation;
use codex_relay_generation::{
    CodexRebindRelayGenerationGate, prepare as prepare_codex_rebind_relay_generation,
    write_message as write_codex_rebind_normalized_message,
};

pub(super) struct RebindRuntimeState {
    pub(super) output_path: String,
    pub(super) synthetic_initial_offset: u64,
    pub(super) input_fifo_path: Option<String>,
    pub(super) runtime_kind: Option<RuntimeHandoffKind>,
    pub(super) session_id: Option<String>,
    pub(super) codex_rollout_path: Option<String>,
    pub(super) codex_rollout_resume_offset: Option<u64>,
    pub(super) codex_rollout_resume_offset_from_marker: bool,
    pub(super) force_initial_offset: Option<u64>,
    pub(super) rebase_existing_offsets_to_output: bool,
}

pub(super) fn resolve_rebind_runtime_state(
    provider: &ProviderKind,
    tmux_session_name: &str,
    existing_saved_output_path: Option<&str>,
    existing_session_id: Option<String>,
) -> Result<RebindRuntimeState, RebindError> {
    let existing_runtime_binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name);
    let observed_runtime_kind = crate::services::tmux_common::resolve_tmux_runtime_kind_marker(
        tmux_session_name,
    )
    .or_else(|| {
        existing_runtime_binding
            .as_ref()
            .map(|binding| binding.runtime_kind)
    });
    if provider == &ProviderKind::Codex
        && observed_runtime_kind == Some(RuntimeHandoffKind::CodexTui)
    {
        // Codex TUI has no wrapper FIFO to rebind. Recovery must adopt the live
        // rollout transcript (or the saved rollout path) and respawn a watcher
        // against that file instead of failing as runtime-unavailable.
        let existing_saved_output_is_normalized_relay = existing_saved_output_path
            .map(|path| codex_rebind_saved_output_is_normalized_relay(tmux_session_name, path))
            .unwrap_or(false);
        let saved_rollout_path = existing_saved_output_path
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .filter(|path| !codex_rebind_saved_output_is_normalized_relay(tmux_session_name, path))
            .filter(|path| std::fs::metadata(path).is_ok())
            .map(str::to_string);
        let codex_rollout_marker =
            crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name)
                .filter(|marker| std::fs::metadata(&marker.rollout_path).is_ok())
                .filter(|marker| {
                    codex_rebind_marker_session_matches(marker, existing_session_id.as_deref())
                });
        let compatible_runtime_binding = existing_runtime_binding
            .as_ref()
            .filter(|binding| binding.runtime_kind == RuntimeHandoffKind::CodexTui)
            .filter(|binding| {
                codex_rebind_runtime_binding_matches_current_turn(
                    binding,
                    codex_rollout_marker.as_ref(),
                    saved_rollout_path.as_deref(),
                    existing_session_id.as_deref(),
                )
            });

        if let Some((binding, output_len)) = compatible_runtime_binding.and_then(|binding| {
            std::fs::metadata(&binding.output_path)
                .ok()
                .map(|metadata| (binding, metadata.len()))
        }) {
            let binding_resume_offset = binding.last_offset.min(output_len);
            let marker_resume_offset = codex_rollout_marker
                .as_ref()
                .and_then(|marker| marker.rollout_start_offset)
                .map(|offset| offset.min(output_len))
                .filter(|offset| {
                    existing_saved_output_is_normalized_relay || *offset >= binding_resume_offset
                });
            let resume_offset =
                if existing_saved_output_is_normalized_relay && marker_resume_offset.is_none() {
                    None
                } else {
                    Some(marker_resume_offset.unwrap_or(binding_resume_offset))
                };
            return Ok(RebindRuntimeState {
                output_path: binding.output_path.clone(),
                synthetic_initial_offset: output_len,
                input_fifo_path: binding.input_fifo_path.clone(),
                runtime_kind: Some(RuntimeHandoffKind::CodexTui),
                session_id: existing_session_id.or_else(|| binding.session_id.clone()),
                codex_rollout_path: Some(binding.output_path.clone()),
                codex_rollout_resume_offset: resume_offset,
                codex_rollout_resume_offset_from_marker: marker_resume_offset.is_some(),
                force_initial_offset: None,
                rebase_existing_offsets_to_output: false,
            });
        }

        if let Some(marker) = codex_rollout_marker
            && let Ok(metadata) = std::fs::metadata(&marker.rollout_path)
        {
            let output_path = marker.rollout_path.display().to_string();
            let output_len = metadata.len();
            let resume_offset = marker
                .rollout_start_offset
                .map(|offset| offset.min(output_len));
            return Ok(RebindRuntimeState {
                output_path: output_path.clone(),
                synthetic_initial_offset: output_len,
                input_fifo_path: None,
                runtime_kind: Some(RuntimeHandoffKind::CodexTui),
                session_id: existing_session_id.clone().or(marker.session_id),
                codex_rollout_path: Some(output_path),
                codex_rollout_resume_offset: resume_offset,
                codex_rollout_resume_offset_from_marker: resume_offset.is_some(),
                force_initial_offset: None,
                rebase_existing_offsets_to_output: false,
            });
        }

        if let Some(saved_output_path) = saved_rollout_path.as_deref() {
            return Ok(RebindRuntimeState {
                output_path: saved_output_path.to_string(),
                synthetic_initial_offset: std::fs::metadata(saved_output_path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0),
                input_fifo_path: None,
                runtime_kind: Some(RuntimeHandoffKind::CodexTui),
                session_id: existing_session_id,
                codex_rollout_path: Some(saved_output_path.to_string()),
                codex_rollout_resume_offset: None,
                codex_rollout_resume_offset_from_marker: false,
                force_initial_offset: None,
                rebase_existing_offsets_to_output: false,
            });
        }

        if let Some(session_id) = existing_session_id.as_deref()
            && let Some(rollout) =
                crate::services::codex_tui::rollout_tail::find_rollout_by_session_id(session_id)
        {
            let output_path = rollout.display().to_string();
            return Ok(RebindRuntimeState {
                synthetic_initial_offset: std::fs::metadata(&output_path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0),
                output_path: output_path.clone(),
                input_fifo_path: None,
                runtime_kind: Some(RuntimeHandoffKind::CodexTui),
                session_id: Some(session_id.to_string()),
                codex_rollout_path: Some(output_path),
                codex_rollout_resume_offset: None,
                codex_rollout_resume_offset_from_marker: false,
                force_initial_offset: None,
                rebase_existing_offsets_to_output: false,
            });
        }

        return Err(RebindError::RuntimeBindingUnavailable {
            tmux_session: tmux_session_name.to_string(),
            runtime_kind: RuntimeHandoffKind::CodexTui,
        });
    }

    if provider == &ProviderKind::Claude
        && observed_runtime_kind == Some(RuntimeHandoffKind::ClaudeTui)
    {
        if let Some(transcript_path) =
            existing_saved_output_path.and_then(claude_rebind_transcript_path)
            && let Ok(metadata) = std::fs::metadata(transcript_path)
        {
            let transcript_session_id = claude_transcript_session_id(transcript_path);
            return Ok(RebindRuntimeState {
                output_path: transcript_path.to_string(),
                synthetic_initial_offset: metadata.len(),
                input_fifo_path: None,
                runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
                session_id: transcript_session_id.or(existing_session_id),
                codex_rollout_path: None,
                codex_rollout_resume_offset: None,
                codex_rollout_resume_offset_from_marker: false,
                force_initial_offset: None,
                rebase_existing_offsets_to_output: false,
            });
        }

        if let Some((binding, output_len)) = existing_runtime_binding
            .as_ref()
            .filter(|binding| binding.runtime_kind == RuntimeHandoffKind::ClaudeTui)
            .filter(|binding| {
                claude_rebind_binding_session_matches(binding, existing_session_id.as_deref())
            })
            .and_then(|binding| {
                std::fs::metadata(&binding.output_path)
                    .ok()
                    .map(|metadata| (binding, metadata.len()))
            })
        {
            let transcript_session_id = claude_transcript_session_id(&binding.output_path);
            return Ok(RebindRuntimeState {
                output_path: binding.output_path.clone(),
                synthetic_initial_offset: output_len,
                input_fifo_path: None,
                runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
                session_id: transcript_session_id
                    .or_else(|| binding.session_id.clone())
                    .or(existing_session_id),
                codex_rollout_path: None,
                codex_rollout_resume_offset: None,
                codex_rollout_resume_offset_from_marker: false,
                force_initial_offset: None,
                rebase_existing_offsets_to_output: false,
            });
        }
    }

    let (default_output_path, default_input_fifo) = tmux_runtime_paths(tmux_session_name);
    let input_fifo_path = Some(default_input_fifo);
    let runtime_kind = observed_runtime_kind;
    let session_id = existing_session_id;
    let fallback_output_path = existing_saved_output_path
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| default_output_path.clone());
    let (output_path, synthetic_initial_offset) = resolve_output_path_for_rebind(
        tmux_session_name,
        &default_output_path,
        &fallback_output_path,
    )?;

    Ok(RebindRuntimeState {
        output_path,
        synthetic_initial_offset,
        input_fifo_path,
        runtime_kind,
        session_id,
        codex_rollout_path: None,
        codex_rollout_resume_offset: None,
        codex_rollout_resume_offset_from_marker: false,
        force_initial_offset: None,
        rebase_existing_offsets_to_output: false,
    })
}

fn codex_rebind_marker_session_matches(
    marker: &crate::services::codex_tui::session::CodexTuiRolloutMarker,
    existing_session_id: Option<&str>,
) -> bool {
    let Some(existing_session_id) = normalized_non_empty(existing_session_id) else {
        return true;
    };
    marker
        .session_id
        .as_deref()
        .and_then(|session_id| normalized_non_empty(Some(session_id)))
        .is_none_or(|marker_session_id| marker_session_id == existing_session_id)
}

fn codex_rebind_runtime_binding_matches_current_turn(
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    marker: Option<&crate::services::codex_tui::session::CodexTuiRolloutMarker>,
    saved_rollout_path: Option<&str>,
    existing_session_id: Option<&str>,
) -> bool {
    let binding_session_id = normalized_non_empty(binding.session_id.as_deref());
    let existing_session_id = normalized_non_empty(existing_session_id);
    if let Some(marker) = marker {
        return codex_rebind_paths_same(&binding.output_path, &marker.rollout_path)
            && codex_rebind_binding_session_matches(
                binding_session_id,
                marker
                    .session_id
                    .as_deref()
                    .and_then(|session_id| normalized_non_empty(Some(session_id))),
                existing_session_id,
            );
    }
    if let Some(saved_rollout_path) = saved_rollout_path {
        return codex_rebind_path_strs_same(&binding.output_path, saved_rollout_path)
            && codex_rebind_binding_session_matches(binding_session_id, None, existing_session_id);
    }
    if let Some(existing_session_id) = existing_session_id {
        return binding_session_id == Some(existing_session_id);
    }
    true
}

fn codex_rebind_binding_session_matches(
    binding_session_id: Option<&str>,
    marker_session_id: Option<&str>,
    existing_session_id: Option<&str>,
) -> bool {
    if let Some(existing_session_id) = existing_session_id
        && binding_session_id.is_some_and(|session_id| session_id != existing_session_id)
    {
        return false;
    }
    if let Some(marker_session_id) = marker_session_id
        && binding_session_id.is_some_and(|session_id| session_id != marker_session_id)
    {
        return false;
    }
    true
}

fn claude_rebind_binding_session_matches(
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    existing_session_id: Option<&str>,
) -> bool {
    let Some(existing_session_id) = normalized_non_empty(existing_session_id) else {
        return true;
    };
    binding
        .session_id
        .as_deref()
        .and_then(|session_id| normalized_non_empty(Some(session_id)))
        .is_none_or(|binding_session_id| binding_session_id == existing_session_id)
}

pub(super) fn claude_rebind_transcript_path(path: &str) -> Option<&str> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let file_name = std::path::Path::new(path).file_name()?.to_str()?;
    let stem = file_name.strip_suffix(".jsonl")?;
    uuid::Uuid::parse_str(stem).is_ok().then_some(path)
}

fn claude_transcript_session_id(path: &str) -> Option<String> {
    let path = claude_rebind_transcript_path(path)?;
    std::path::Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string)
}

fn normalized_non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn codex_rebind_path_strs_same(left: &str, right: &str) -> bool {
    codex_rebind_paths_same(left, std::path::Path::new(right))
}

fn codex_rebind_paths_same(left: &str, right: &std::path::Path) -> bool {
    let left = std::path::Path::new(left);
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn codex_rebind_saved_output_is_normalized_relay(
    tmux_session_name: &str,
    saved_output_path: &str,
) -> bool {
    let relay_output_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    std::path::Path::new(saved_output_path) == std::path::Path::new(&relay_output_path)
}

fn persist_codex_tui_rebind_rollout_cursor(
    tmux_session_name: &str,
    rollout_path: &std::path::Path,
    session_id: Option<&str>,
    raw_offset: u64,
) {
    let durable_offset =
        crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name)
            .filter(|marker| {
                codex_rebind_paths_same(&rollout_path.display().to_string(), &marker.rollout_path)
            })
            .and_then(|marker| marker.rollout_start_offset)
            .map(|existing| existing.max(raw_offset))
            .unwrap_or(raw_offset);

    if let Err(error) =
        crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
            tmux_session_name,
            rollout_path,
            session_id,
            Some(durable_offset),
        )
    {
        tracing::warn!(
            tmux_session_name,
            rollout_path = %rollout_path.display(),
            raw_offset = durable_offset,
            error,
            "failed to persist Codex TUI rebind rollout cursor; restart may replay already-normalized rollout frames"
        );
    }
}

pub(super) fn spawn_codex_tui_rebind_relay_output(
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
        prepare_codex_rebind_relay_generation(&relay_output_path, truncate_relay_output)?;

    persist_codex_tui_rebind_rollout_cursor(
        tmux_session_name,
        std::path::Path::new(rollout_path),
        session_id.as_deref(),
        raw_start_offset,
    );
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        tmux_session_name,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: rollout_path.to_string(),
            relay_output_path: Some(relay_output_path.clone()),
            input_fifo_path: None,
            session_id: session_id.clone(),
            last_offset: raw_start_offset,
            relay_last_offset: Some(0),
        },
    );

    let tmux_session_name = tmux_session_name.to_string();
    let rollout_path = std::path::PathBuf::from(rollout_path);
    let relay_path = std::path::PathBuf::from(&relay_output_path);
    let watcher_cancel_for_writer = watcher_cancel.clone();
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
                    crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset(
                        &tail_rollout_path,
                        raw_start_offset,
                        tail_session_id.as_deref(),
                        sender,
                        Some(tail_cancel_token),
                        || {
                            !watcher_cancel_for_alive
                                .load(std::sync::atomic::Ordering::Relaxed)
                                && crate::services::tmux_diagnostics::tmux_session_has_live_pane(
                                    &tail_tmux_session_name,
                                )
                        },
                    )
                });

            let writer_result = write_codex_rebind_normalized_stream_for_generation(
                &relay_path,
                receiver,
                already_relayed_response,
                already_normalized_replay_events,
                &relay_generation_gate,
                relay_generation,
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
                            crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
                                &tmux_session_name,
                                rollout_path.to_str().unwrap_or_default(),
                                final_offset,
                            );
                            persist_codex_tui_rebind_rollout_cursor(
                                &tmux_session_name,
                                &rollout_path,
                                session_id.as_deref(),
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

#[cfg(test)]
fn write_codex_rebind_normalized_stream(
    relay_path: &std::path::Path,
    receiver: std::sync::mpsc::Receiver<crate::services::agent_protocol::StreamMessage>,
    already_relayed_response: String,
    already_normalized_replay_events: Vec<serde_json::Value>,
) -> Result<(), String> {
    let relay_path_string = relay_path.to_string_lossy();
    let (relay_generation_gate, relay_generation) =
        prepare_codex_rebind_relay_generation(&relay_path_string, false)
            .map_err(|error| format!("prepare test relay generation: {error}"))?;
    write_codex_rebind_normalized_stream_for_generation(
        relay_path,
        receiver,
        already_relayed_response,
        already_normalized_replay_events,
        &relay_generation_gate,
        relay_generation,
    )
}

fn write_codex_rebind_normalized_stream_for_generation(
    relay_path: &std::path::Path,
    receiver: std::sync::mpsc::Receiver<crate::services::agent_protocol::StreamMessage>,
    already_relayed_response: String,
    already_normalized_replay_events: Vec<serde_json::Value>,
    relay_generation_gate: &CodexRebindRelayGenerationGate,
    relay_generation: u64,
) -> Result<(), String> {
    let mut already_relayed_response = already_relayed_response;
    let mut known_response_for_done = already_relayed_response.clone();
    let mut already_normalized_replay_events =
        std::collections::VecDeque::from(already_normalized_replay_events);
    {
        let current_generation = relay_generation_gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if *current_generation != relay_generation {
            return Err(
                "Codex TUI rebind relay generation was superseded before writer start".to_string(),
            );
        }
        codex_rebind_ensure_jsonl_append_boundary(relay_path)?;
    }
    let mut output = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(relay_path)
        .map_err(|error| format!("open {}: {error}", relay_path.display()))?;
    for message in receiver {
        let Some(message) =
            codex_rebind_message_after_relayed_prefix(message, &mut already_relayed_response)
        else {
            continue;
        };
        if let crate::services::agent_protocol::StreamMessage::Done { result, .. } = &message
            && let Some(suffix) = codex_rebind_done_result_suffix(&known_response_for_done, result)
        {
            let suffix_message = crate::services::agent_protocol::StreamMessage::Text {
                content: suffix.clone(),
            };
            write_codex_rebind_normalized_message(
                &mut output,
                relay_path,
                suffix_message,
                &mut already_normalized_replay_events,
                relay_generation_gate,
                relay_generation,
            )?;
            known_response_for_done.push_str(&suffix);
        }
        let forwarded_text = match &message {
            crate::services::agent_protocol::StreamMessage::Text { content } => {
                Some(content.clone())
            }
            _ => None,
        };
        write_codex_rebind_normalized_message(
            &mut output,
            relay_path,
            message,
            &mut already_normalized_replay_events,
            relay_generation_gate,
            relay_generation,
        )?;
        if let Some(text) = forwarded_text {
            known_response_for_done.push_str(&text);
        }
    }
    Ok(())
}

fn codex_rebind_done_result_suffix(known_response: &str, result: &str) -> Option<String> {
    if known_response.is_empty() || !result.starts_with(known_response) {
        return None;
    }
    let suffix = &result[known_response.len()..];
    (!suffix.trim().is_empty()).then(|| suffix.to_string())
}

fn codex_rebind_ensure_jsonl_append_boundary(relay_path: &std::path::Path) -> Result<(), String> {
    use std::io::{Read, Seek, Write};

    let metadata = match std::fs::metadata(relay_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("stat {}: {error}", relay_path.display())),
    };
    if !metadata.is_file() || metadata.len() == 0 {
        return Ok(());
    }

    let mut output = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(relay_path)
        .map_err(|error| format!("open {}: {error}", relay_path.display()))?;
    output
        .seek(std::io::SeekFrom::End(-1))
        .map_err(|error| format!("seek {}: {error}", relay_path.display()))?;
    let mut last = [0_u8; 1];
    output
        .read_exact(&mut last)
        .map_err(|error| format!("read {}: {error}", relay_path.display()))?;
    if last[0] != b'\n' {
        output
            .write_all(b"\n")
            .and_then(|_| output.flush())
            .map_err(|error| format!("write boundary {}: {error}", relay_path.display()))?;
    }
    Ok(())
}

fn codex_rebind_should_skip_existing_normalized_event(
    json: &serde_json::Value,
    already_normalized_replay_events: &mut std::collections::VecDeque<serde_json::Value>,
) -> bool {
    let Some(expected) = already_normalized_replay_events.front() else {
        return false;
    };
    if expected == json {
        already_normalized_replay_events.pop_front();
        return true;
    }

    if let Some(position) = already_normalized_replay_events
        .iter()
        .position(|expected| expected == json)
    {
        let skipped_prefix = position;
        for _ in 0..=position {
            already_normalized_replay_events.pop_front();
        }
        tracing::debug!(
            skipped_prefix,
            actual = %json,
            remaining_replay_events = already_normalized_replay_events.len(),
            "Codex TUI rebind normalized replay advanced past existing relay prefix"
        );
        return true;
    }

    tracing::warn!(
        expected = %expected,
        actual = %json,
        remaining_replay_events = already_normalized_replay_events.len(),
        "Codex TUI rebind normalized replay diverged from existing relay prefix; forwarding subsequent events"
    );
    already_normalized_replay_events.clear();
    false
}

fn codex_rebind_message_after_relayed_prefix(
    message: crate::services::agent_protocol::StreamMessage,
    already_relayed_response: &mut String,
) -> Option<crate::services::agent_protocol::StreamMessage> {
    if already_relayed_response.is_empty() {
        return Some(message);
    }
    let crate::services::agent_protocol::StreamMessage::Text { content } = message else {
        return match message {
            crate::services::agent_protocol::StreamMessage::ToolUse { .. }
            | crate::services::agent_protocol::StreamMessage::ToolResult { .. }
            | crate::services::agent_protocol::StreamMessage::Thinking { .. }
            | crate::services::agent_protocol::StreamMessage::TaskNotification { .. } => {
                tracing::debug!(
                    expected_prefix_len = already_relayed_response.len(),
                    "Codex TUI rebind relay suppressed replayed non-text event before already-relayed text prefix"
                );
                None
            }
            other => Some(other),
        };
    };
    if already_relayed_response.starts_with(&content) {
        already_relayed_response.drain(..content.len());
        return None;
    }
    if content.starts_with(already_relayed_response.as_str()) {
        let suffix = content[already_relayed_response.len()..].to_string();
        already_relayed_response.clear();
        if suffix.is_empty() {
            return None;
        }
        return Some(crate::services::agent_protocol::StreamMessage::Text { content: suffix });
    }
    tracing::warn!(
        expected_prefix_len = already_relayed_response.len(),
        content_len = content.len(),
        "Codex TUI rebind relay could not align already-relayed response prefix; forwarding text"
    );
    already_relayed_response.clear();
    Some(crate::services::agent_protocol::StreamMessage::Text { content })
}

fn codex_rebind_stream_message_json(
    message: crate::services::agent_protocol::StreamMessage,
) -> Option<serde_json::Value> {
    match message {
        crate::services::agent_protocol::StreamMessage::Init { session_id, .. } => {
            Some(serde_json::json!({
                "type": "system",
                "subtype": "init",
                "session_id": session_id,
            }))
        }
        crate::services::agent_protocol::StreamMessage::Text { content } => {
            Some(serde_json::json!({
                "type": "assistant",
                "message": {
                    "content": [{
                        "type": "text",
                        "text": content,
                    }]
                }
            }))
        }
        crate::services::agent_protocol::StreamMessage::ToolUse {
            name,
            input,
            tool_use_id,
        } => Some(serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "id": tool_use_id,
                    "tool_use_id": tool_use_id,
                    "name": name,
                    "input": codex_rebind_tool_input_value(input),
                }]
            }
        })),
        crate::services::agent_protocol::StreamMessage::ToolResult {
            content,
            is_error,
            tool_use_id,
        } => Some(serde_json::json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                    "is_error": is_error,
                }]
            }
        })),
        crate::services::agent_protocol::StreamMessage::Thinking { .. } => {
            Some(serde_json::json!({
                "type": "assistant",
                "message": {
                    "content": [{
                        "type": "thinking",
                    }]
                }
            }))
        }
        crate::services::agent_protocol::StreamMessage::TaskNotification {
            task_id,
            tool_use_id,
            status,
            summary,
            kind,
        } => Some(serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": task_id,
            "tool_use_id": tool_use_id,
            "status": status,
            "summary": summary,
            "task_notification_kind": kind.as_str(),
        })),
        crate::services::agent_protocol::StreamMessage::Done { result, session_id } => {
            Some(serde_json::json!({
                "type": "result",
                "subtype": "success",
                "result": result,
                "session_id": session_id,
            }))
        }
        crate::services::agent_protocol::StreamMessage::Error {
            message, stderr, ..
        } => {
            let result = if stderr.trim().is_empty() {
                message
            } else {
                format!("{message}\n{}", stderr.trim())
            };
            Some(serde_json::json!({
                "type": "result",
                "subtype": "error",
                "is_error": true,
                "result": result.clone(),
                "errors": [result],
            }))
        }
        crate::services::agent_protocol::StreamMessage::StatusUpdate {
            duration_ms,
            num_turns,
            input_tokens,
            cache_create_tokens,
            cache_read_tokens,
            output_tokens,
            ..
        } => Some(serde_json::json!({
            "type": "system",
            "subtype": "turn_duration",
            "duration_ms": duration_ms,
            "num_turns": num_turns,
            "input_tokens": input_tokens,
            "cache_creation_input_tokens": cache_create_tokens,
            "cache_read_input_tokens": cache_read_tokens,
            "output_tokens": output_tokens,
        })),
        crate::services::agent_protocol::StreamMessage::StatusEvents { .. }
        | crate::services::agent_protocol::StreamMessage::RetryBoundary
        | crate::services::agent_protocol::StreamMessage::TmuxReady { .. }
        | crate::services::agent_protocol::StreamMessage::RuntimeReady { .. }
        | crate::services::agent_protocol::StreamMessage::ProcessReady { .. }
        | crate::services::agent_protocol::StreamMessage::OutputOffset { .. } => None,
    }
}

fn codex_rebind_tool_input_value(input: String) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(&input)
        .unwrap_or_else(|_| serde_json::Value::String(input))
}

fn resolve_output_path_for_rebind(
    tmux_session_name: &str,
    default_output_path: &str,
    fallback_output_path: &str,
) -> Result<(String, u64), RebindError> {
    #[cfg(unix)]
    {
        match detect_live_tmux_output_path(tmux_session_name, fallback_output_path) {
            Ok(Some(detected)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ♻ rebind adopted live tmux output path for {}: {} -> {} (offset {})",
                    tmux_session_name,
                    default_output_path,
                    detected.path,
                    detected.initial_offset
                );
                Ok((detected.path, detected.initial_offset))
            }
            Ok(None) => Ok((
                fallback_output_path.to_string(),
                std::fs::metadata(fallback_output_path)
                    .map(|m| m.len())
                    .unwrap_or(0),
            )),
            Err(stale) => Err(RebindError::StaleOutputPath {
                tmux_session: tmux_session_name.to_string(),
                output_path: fallback_output_path.to_string(),
                live_fd: stale.fd,
                live_inode: stale.inode,
                live_path: stale.raw_path,
            }),
        }
    }
    #[cfg(not(unix))]
    {
        Ok((
            fallback_output_path.to_string(),
            std::fs::metadata(fallback_output_path)
                .map(|m| m.len())
                .unwrap_or(0),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_prompt_dedupe::TuiRuntimeBinding;
    use std::ffi::OsString;
    use std::path::Path;

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set_path(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn lock_test_env() -> std::sync::MutexGuard<'static, ()> {
        crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn lock_tui_prompt_dedupe() -> std::sync::MutexGuard<'static, ()> {
        crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn write_codex_tui_runtime_marker(tmux_session_name: &str) {
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            tmux_session_name,
            RuntimeHandoffKind::CodexTui,
        )
        .expect("write runtime kind marker");
    }

    fn write_runtime_kind_marker(tmux_session_name: &str, runtime_kind: RuntimeHandoffKind) {
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            tmux_session_name,
            runtime_kind,
        )
        .expect("write runtime kind marker");
    }

    fn write_rollout(path: &Path) -> u64 {
        let body = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"sess-1\"}}\n{\"type\":\"response\"}\n";
        std::fs::write(path, body).expect("write rollout");
        body.len() as u64
    }

    fn assert_codex_rollout_path(result: &RebindRuntimeState, rollout_path: &Path) {
        assert_eq!(
            result.codex_rollout_path.as_deref(),
            Some(rollout_path.to_str().expect("utf8 rollout path"))
        );
        assert_eq!(result.force_initial_offset, None);
    }

    #[test]
    fn claude_rebind_with_selector_transcript_uses_claude_tui_output_path() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-claude-adk-cc-manual-rebind";
        write_runtime_kind_marker(tmux_session_name, RuntimeHandoffKind::ClaudeTui);
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let session_id = "c62c2dc8-0000-4000-8000-000000000000";
        let transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            session_id,
            Some(claude_home.path()),
        )
        .expect("transcript path");
        std::fs::create_dir_all(transcript_path.parent().expect("transcript parent"))
            .expect("create transcript parent");
        std::fs::write(&transcript_path, b"fresh transcript\n").expect("write transcript");

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Claude,
            tmux_session_name,
            Some(transcript_path.to_str().expect("utf8 transcript path")),
            Some(session_id.to_string()),
        )
        .expect("claude rebind with selector transcript should use Claude TUI output path");

        assert_eq!(result.output_path, transcript_path.display().to_string());
        assert_eq!(
            result.synthetic_initial_offset,
            b"fresh transcript\n".len() as u64
        );
        assert_eq!(result.input_fifo_path, None);
        assert_eq!(result.runtime_kind, Some(RuntimeHandoffKind::ClaudeTui));
        assert_eq!(result.session_id.as_deref(), Some(session_id));
        assert_eq!(result.codex_rollout_path, None);
    }

    #[test]
    fn claude_rebind_with_legacy_runtime_marker_does_not_adopt_transcript_as_claude_tui() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-claude-adk-legacy-manual-rebind";
        write_runtime_kind_marker(tmux_session_name, RuntimeHandoffKind::LegacyTmuxWrapper);
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let session_id = "c62c2dc8-0000-4000-8000-000000000000";
        let transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            session_id,
            Some(claude_home.path()),
        )
        .expect("transcript path");
        std::fs::create_dir_all(transcript_path.parent().expect("transcript parent"))
            .expect("create transcript parent");
        std::fs::write(&transcript_path, b"legacy transcript\n").expect("write transcript");

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Claude,
            tmux_session_name,
            Some(transcript_path.to_str().expect("utf8 transcript path")),
            Some(session_id.to_string()),
        )
        .expect("legacy Claude wrapper rebind should retain wrapper semantics");

        assert_eq!(
            result.runtime_kind,
            Some(RuntimeHandoffKind::LegacyTmuxWrapper)
        );
        assert!(
            result.input_fifo_path.is_some(),
            "legacy wrapper rebind must not lose its input FIFO"
        );
        assert_ne!(
            result.runtime_kind,
            Some(RuntimeHandoffKind::ClaudeTui),
            "a transcript candidate alone must not promote LegacyTmuxWrapper to ClaudeTui"
        );
    }

    #[test]
    fn claude_rebind_without_runtime_marker_does_not_promote_transcript_to_claude_tui() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-claude-adk-unknown-kind-manual-rebind";
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let session_id = "c62c2dc8-0000-4000-8000-000000000000";
        let transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            session_id,
            Some(claude_home.path()),
        )
        .expect("transcript path");
        std::fs::create_dir_all(transcript_path.parent().expect("transcript parent"))
            .expect("create transcript parent");
        std::fs::write(&transcript_path, b"unknown-kind transcript\n").expect("write transcript");

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Claude,
            tmux_session_name,
            Some(transcript_path.to_str().expect("utf8 transcript path")),
            Some(session_id.to_string()),
        )
        .expect("unknown runtime kind should keep non-ClaudeTui semantics");

        assert_eq!(result.runtime_kind, None);
        assert!(
            result.input_fifo_path.is_some(),
            "unknown runtime kind fails closed instead of dropping FIFO semantics"
        );
    }

    #[test]
    fn claude_rebind_adopted_transcript_session_id_follows_transcript_stem() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-claude-adk-raw-session-id-rebind";
        write_runtime_kind_marker(tmux_session_name, RuntimeHandoffKind::ClaudeTui);
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let claude_home = tempfile::tempdir().expect("claude home tempdir");
        let cached_session_id = "c62c2dc8-0000-4000-8000-000000000000";
        let raw_session_id = "48fdb7f3-0000-4000-8000-000000000000";
        let transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            raw_session_id,
            Some(claude_home.path()),
        )
        .expect("raw transcript path");
        std::fs::create_dir_all(transcript_path.parent().expect("transcript parent"))
            .expect("create transcript parent");
        std::fs::write(&transcript_path, b"raw transcript\n").expect("write transcript");

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Claude,
            tmux_session_name,
            Some(transcript_path.to_str().expect("utf8 transcript path")),
            Some(cached_session_id.to_string()),
        )
        .expect("claude rebind should adopt raw transcript");

        assert_eq!(result.output_path, transcript_path.display().to_string());
        assert_eq!(
            result.session_id.as_deref(),
            Some(raw_session_id),
            "adopting raw transcript B must not keep stale cached session id A"
        );
    }

    #[test]
    fn claude_rebind_without_transcript_candidate_uses_default_wrapper_output_path() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-claude-adk-cc-manual-rebind-no-transcript";

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Claude,
            tmux_session_name,
            None,
            Some("c62c2dc8-0000-4000-8000-000000000000".to_string()),
        )
        .expect("claude rebind without transcript candidate should use wrapper output path");

        assert_eq!(
            result.output_path,
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl")
        );
        assert_eq!(result.synthetic_initial_offset, 0);
        assert!(result.input_fifo_path.is_some());
        assert_eq!(result.runtime_kind, None);
        assert_eq!(
            result.session_id.as_deref(),
            Some("c62c2dc8-0000-4000-8000-000000000000")
        );
        assert_eq!(result.codex_rollout_path, None);
    }

    #[test]
    fn direct_codex_tui_rebind_adopts_existing_runtime_binding_and_clamps_offset() {
        let _guard = lock_test_env();
        let _dedupe_guard = lock_tui_prompt_dedupe();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let rollout_path = tmp.path().join("rollout.jsonl");
        let rollout_len = write_rollout(&rollout_path);
        let tmux_session_name = "AgentDesk-codex-adk-cdx-existing-runtime-binding";
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux_session_name,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: rollout_path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("existing-session".to_string()),
                last_offset: rollout_len + 10,
                relay_last_offset: None,
            },
        );

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Codex,
            tmux_session_name,
            None,
            Some("existing-session".to_string()),
        )
        .expect("codex tui rebind should adopt live rollout binding");

        assert_eq!(result.output_path, rollout_path.display().to_string());
        assert_eq!(result.synthetic_initial_offset, rollout_len);
        assert_eq!(result.input_fifo_path, None);
        assert_eq!(result.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
        assert_eq!(result.session_id.as_deref(), Some("existing-session"));
        assert_codex_rollout_path(&result, &rollout_path);
        assert_eq!(result.codex_rollout_resume_offset, Some(rollout_len));
        assert!(!result.codex_rollout_resume_offset_from_marker);
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
    }

    #[test]
    fn direct_codex_tui_rebind_uses_rollout_marker_offset_before_binding_eof() {
        let _guard = lock_test_env();
        let _dedupe_guard = lock_tui_prompt_dedupe();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let rollout_path = tmp.path().join("marker-offset-rollout.jsonl");
        let rollout_len = write_rollout(&rollout_path);
        let tmux_session_name = "AgentDesk-codex-adk-cdx-marker-offset";
        crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
            tmux_session_name,
            &rollout_path,
            Some("marker-offset-session"),
            Some(12),
        )
        .expect("write rollout marker");
        let relay_path =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        std::fs::create_dir_all(std::path::Path::new(&relay_path).parent().unwrap())
            .expect("create relay parent");
        std::fs::write(&relay_path, "{\"type\":\"assistant\"}\n").expect("write relay");
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux_session_name,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: rollout_path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("marker-offset-session".to_string()),
                last_offset: rollout_len,
                relay_last_offset: None,
            },
        );

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Codex,
            tmux_session_name,
            Some(&relay_path),
            Some("marker-offset-session".to_string()),
        )
        .expect("codex tui rebind should prefer durable rollout marker offset");

        assert_eq!(result.output_path, rollout_path.display().to_string());
        assert_eq!(result.synthetic_initial_offset, rollout_len);
        assert_codex_rollout_path(&result, &rollout_path);
        assert_eq!(result.codex_rollout_resume_offset, Some(12));
        assert!(result.codex_rollout_resume_offset_from_marker);
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
    }

    #[test]
    fn direct_codex_tui_rebind_ignores_stale_marker_before_raw_binding_cursor() {
        let _guard = lock_test_env();
        let _dedupe_guard = lock_tui_prompt_dedupe();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let rollout_path = tmp.path().join("stale-marker-rollout.jsonl");
        let rollout_len = write_rollout(&rollout_path);
        let tmux_session_name = "AgentDesk-codex-adk-cdx-stale-marker";
        crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
            tmux_session_name,
            &rollout_path,
            Some("stale-marker-session"),
            Some(12),
        )
        .expect("write rollout marker");
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux_session_name,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: rollout_path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("stale-marker-session".to_string()),
                last_offset: rollout_len,
                relay_last_offset: None,
            },
        );

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Codex,
            tmux_session_name,
            Some(rollout_path.to_str().expect("utf8 rollout path")),
            Some("stale-marker-session".to_string()),
        )
        .expect("codex tui rebind should not let stale markers rewind raw cursors");

        assert_eq!(result.output_path, rollout_path.display().to_string());
        assert_eq!(result.synthetic_initial_offset, rollout_len);
        assert_codex_rollout_path(&result, &rollout_path);
        assert_eq!(result.codex_rollout_resume_offset, Some(rollout_len));
        assert!(!result.codex_rollout_resume_offset_from_marker);
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
    }

    #[test]
    fn direct_codex_tui_rebind_respawns_writer_for_existing_normalized_relay_output() {
        let _guard = lock_test_env();
        let _dedupe_guard = lock_tui_prompt_dedupe();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let rollout_path = tmp.path().join("rollout.jsonl");
        let rollout_len = write_rollout(&rollout_path);
        let relay_path = tmp.path().join("relay.jsonl");
        std::fs::write(
            &relay_path,
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"relayed\"}]}}\n",
        )
        .expect("write relay output");
        let tmux_session_name = "AgentDesk-codex-adk-cdx-relay";
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux_session_name,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: rollout_path.display().to_string(),
                relay_output_path: Some(relay_path.display().to_string()),
                input_fifo_path: None,
                session_id: Some("relay-session".to_string()),
                last_offset: 12,
                relay_last_offset: Some(5),
            },
        );

        let result =
            resolve_rebind_runtime_state(&ProviderKind::Codex, tmux_session_name, None, None)
                .expect("codex tui rebind should respawn normalized relay output");

        assert_eq!(result.output_path, rollout_path.display().to_string());
        assert_eq!(result.synthetic_initial_offset, rollout_len);
        assert_eq!(result.input_fifo_path, None);
        assert_eq!(result.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
        assert_eq!(result.session_id.as_deref(), Some("relay-session"));
        assert_codex_rollout_path(&result, &rollout_path);
        assert_eq!(result.codex_rollout_resume_offset, Some(12));
        assert!(!result.rebase_existing_offsets_to_output);
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
    }

    #[test]
    fn direct_codex_tui_rebind_does_not_use_eof_binding_for_empty_normalized_relay() {
        let _guard = lock_test_env();
        let _dedupe_guard = lock_tui_prompt_dedupe();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let rollout_path = tmp.path().join("rollout.jsonl");
        let rollout_len = write_rollout(&rollout_path);
        let tmux_session_name = "AgentDesk-codex-adk-cdx-empty-relay";
        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
            tmux_session_name,
            &rollout_path,
            Some("relay-session"),
        )
        .expect("write legacy marker without start offset");
        let relay_path =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux_session_name,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: rollout_path.display().to_string(),
                relay_output_path: Some(relay_path.clone()),
                input_fifo_path: None,
                session_id: Some("relay-session".to_string()),
                last_offset: rollout_len,
                relay_last_offset: Some(0),
            },
        );

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Codex,
            tmux_session_name,
            Some(&relay_path),
            Some("relay-session".to_string()),
        )
        .expect("codex tui rebind should rebuild an empty normalized relay");

        assert_eq!(result.output_path, rollout_path.display().to_string());
        assert_eq!(result.synthetic_initial_offset, rollout_len);
        assert_codex_rollout_path(&result, &rollout_path);
        assert_eq!(
            result.codex_rollout_resume_offset, None,
            "without a marker start offset, an EOF runtime binding must not skip prompt-boundary replay for normalized relay rebuild"
        );
        assert!(!result.codex_rollout_resume_offset_from_marker);
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
    }

    #[test]
    fn direct_codex_tui_rebind_uses_saved_output_path_after_restart() {
        let _guard = lock_test_env();
        let _dedupe_guard = lock_tui_prompt_dedupe();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let rollout_path = tmp.path().join("saved-rollout.jsonl");
        let rollout_len = write_rollout(&rollout_path);
        let tmux_session_name = "AgentDesk-codex-adk-cdx-saved";
        write_codex_tui_runtime_marker(tmux_session_name);

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Codex,
            tmux_session_name,
            Some(rollout_path.to_str().expect("utf8 rollout path")),
            Some("saved-session".to_string()),
        )
        .expect("codex tui rebind should adopt saved rollout path after restart");

        assert_eq!(result.output_path, rollout_path.display().to_string());
        assert_eq!(result.synthetic_initial_offset, rollout_len);
        assert_eq!(result.input_fifo_path, None);
        assert_eq!(result.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
        assert_eq!(result.session_id.as_deref(), Some("saved-session"));
        assert_codex_rollout_path(&result, &rollout_path);
        assert_eq!(result.codex_rollout_resume_offset, None);
    }

    #[test]
    fn direct_codex_tui_rebind_rejects_existing_binding_for_old_rollout() {
        let _guard = lock_test_env();
        let _dedupe_guard = lock_tui_prompt_dedupe();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let stale_rollout_path = tmp.path().join("old-rollout.jsonl");
        write_rollout(&stale_rollout_path);
        let saved_rollout_path = tmp.path().join("saved-rollout.jsonl");
        let saved_rollout_len = write_rollout(&saved_rollout_path);
        let tmux_session_name = "AgentDesk-codex-adk-cdx-stale-existing-binding";
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux_session_name,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: stale_rollout_path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("old-session".to_string()),
                last_offset: 12,
                relay_last_offset: None,
            },
        );

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Codex,
            tmux_session_name,
            Some(saved_rollout_path.to_str().expect("utf8 rollout path")),
            Some("saved-session".to_string()),
        )
        .expect("codex tui rebind should ignore a live but stale runtime binding");

        assert_eq!(result.output_path, saved_rollout_path.display().to_string());
        assert_eq!(result.synthetic_initial_offset, saved_rollout_len);
        assert_eq!(result.input_fifo_path, None);
        assert_eq!(result.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
        assert_eq!(result.session_id.as_deref(), Some("saved-session"));
        assert_codex_rollout_path(&result, &saved_rollout_path);
        assert_eq!(result.codex_rollout_resume_offset, None);
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
    }

    #[test]
    fn direct_codex_tui_rebind_falls_back_from_stale_binding_to_saved_output_path() {
        let _guard = lock_test_env();
        let _dedupe_guard = lock_tui_prompt_dedupe();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let rollout_path = tmp.path().join("saved-rollout.jsonl");
        let rollout_len = write_rollout(&rollout_path);
        let tmux_session_name = "AgentDesk-codex-adk-cdx-stale-binding";
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux_session_name,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: tmp
                    .path()
                    .join("missing-rollout.jsonl")
                    .display()
                    .to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("stale-binding-session".to_string()),
                last_offset: 99,
                relay_last_offset: None,
            },
        );

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Codex,
            tmux_session_name,
            Some(rollout_path.to_str().expect("utf8 rollout path")),
            None,
        )
        .expect("codex tui rebind should ignore stale binding and adopt saved rollout path");

        assert_eq!(result.output_path, rollout_path.display().to_string());
        assert_eq!(result.synthetic_initial_offset, rollout_len);
        assert_eq!(result.input_fifo_path, None);
        assert_eq!(result.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
        assert_eq!(result.session_id, None);
        assert_codex_rollout_path(&result, &rollout_path);
        assert_eq!(result.codex_rollout_resume_offset, None);
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
    }

    #[test]
    fn direct_codex_tui_rebind_uses_rollout_marker_before_saved_normalized_relay() {
        let _guard = lock_test_env();
        let _dedupe_guard = lock_tui_prompt_dedupe();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let rollout_path = tmp.path().join("marker-rollout.jsonl");
        let rollout_len = write_rollout(&rollout_path);
        let tmux_session_name = "AgentDesk-codex-adk-cdx-marker";
        write_codex_tui_runtime_marker(tmux_session_name);
        crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
            tmux_session_name,
            &rollout_path,
            Some("marker-session"),
            Some(12),
        )
        .expect("write rollout marker");
        let relay_path =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        std::fs::create_dir_all(std::path::Path::new(&relay_path).parent().unwrap())
            .expect("create relay parent");
        std::fs::write(&relay_path, "{\"type\":\"assistant\"}\n").expect("write relay");
        let stale_rollout_path = tmp.path().join("old-marker-rollout.jsonl");
        write_rollout(&stale_rollout_path);
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux_session_name,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: stale_rollout_path.display().to_string(),
                relay_output_path: Some(relay_path.clone()),
                input_fifo_path: None,
                session_id: Some("old-marker-session".to_string()),
                last_offset: 99,
                relay_last_offset: Some(3),
            },
        );

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Codex,
            tmux_session_name,
            Some(&relay_path),
            None,
        )
        .expect("codex tui rebind should use raw rollout marker, not saved normalized relay");

        assert_eq!(result.output_path, rollout_path.display().to_string());
        assert_eq!(result.synthetic_initial_offset, rollout_len);
        assert_eq!(result.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
        assert_eq!(result.session_id.as_deref(), Some("marker-session"));
        assert_codex_rollout_path(&result, &rollout_path);
        assert_eq!(result.codex_rollout_resume_offset, Some(12));
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
    }

    #[test]
    fn direct_codex_tui_rebind_finds_rollout_by_session_id_after_restart() {
        let _guard = lock_test_env();
        let _dedupe_guard = lock_tui_prompt_dedupe();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmp = tempfile::tempdir().expect("tempdir");
        let codex_home = tmp.path().join("codex-home");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let _codex_home = EnvGuard::set_path("CODEX_HOME", &codex_home);
        let sessions_dir = codex_home
            .join("sessions")
            .join("2026")
            .join("06")
            .join("29");
        std::fs::create_dir_all(&sessions_dir).expect("create sessions dir");
        let session_id = "019f10e3-3dad-73c2-9d8c-e6188e4ccc7c";
        let rollout_path =
            sessions_dir.join(format!("rollout-2026-06-29T09-59-13-{session_id}.jsonl"));
        let rollout_len = write_rollout(&rollout_path);
        let tmux_session_name = "AgentDesk-codex-adk-cdx-session";
        write_codex_tui_runtime_marker(tmux_session_name);

        let result = resolve_rebind_runtime_state(
            &ProviderKind::Codex,
            tmux_session_name,
            None,
            Some(session_id.to_string()),
        )
        .expect("codex tui rebind should find rollout by session id after restart");

        assert_eq!(result.output_path, rollout_path.display().to_string());
        assert_eq!(result.synthetic_initial_offset, rollout_len);
        assert_eq!(result.input_fifo_path, None);
        assert_eq!(result.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
        assert_eq!(result.session_id.as_deref(), Some(session_id));
        assert_codex_rollout_path(&result, &rollout_path);
        assert_eq!(result.codex_rollout_resume_offset, None);
    }

    #[test]
    fn codex_rebind_rollout_cursor_persists_explicit_monotonic_offset() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let rollout_path = tmp.path().join("cursor-rollout.jsonl");
        write_rollout(&rollout_path);
        let tmux_session_name = "AgentDesk-codex-adk-cdx-cursor";

        persist_codex_tui_rebind_rollout_cursor(
            tmux_session_name,
            &rollout_path,
            Some("cursor-session"),
            12,
        );
        persist_codex_tui_rebind_rollout_cursor(
            tmux_session_name,
            &rollout_path,
            Some("cursor-session"),
            88,
        );

        let marker =
            crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name)
                .expect("marker");
        assert_eq!(marker.rollout_path, rollout_path);
        assert_eq!(marker.session_id.as_deref(), Some("cursor-session"));
        assert_eq!(marker.rollout_start_offset, Some(88));
    }

    #[test]
    fn codex_rebind_rollout_cursor_does_not_move_backward_for_same_rollout() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let rollout_path = tmp.path().join("cursor-rollout.jsonl");
        write_rollout(&rollout_path);
        let tmux_session_name = "AgentDesk-codex-adk-cdx-cursor-monotonic";

        persist_codex_tui_rebind_rollout_cursor(
            tmux_session_name,
            &rollout_path,
            Some("cursor-session"),
            88,
        );
        persist_codex_tui_rebind_rollout_cursor(
            tmux_session_name,
            &rollout_path,
            Some("cursor-session"),
            12,
        );

        let marker =
            crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name)
                .expect("marker");
        assert_eq!(marker.rollout_start_offset, Some(88));
    }

    /// #4455: forced same-path watcher replacement cancels the incumbent, but
    /// its detached converter thread can still have buffered output. The relay
    /// generation gate must fence that writer before the replacement truncates
    /// and begins a fresh Discord surface.
    #[test]
    fn codex_rebind_relay_generation_fences_superseded_same_path_writer() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay_path = tmp.path().join("relay.jsonl");
        let relay_path_string = relay_path.to_string_lossy();
        let (gate, old_generation) =
            prepare_codex_rebind_relay_generation(&relay_path_string, true)
                .expect("prepare old generation");
        let mut old_output = std::fs::OpenOptions::new()
            .append(true)
            .open(&relay_path)
            .expect("open old generation output");
        let (_, replacement_generation) =
            prepare_codex_rebind_relay_generation(&relay_path_string, true)
                .expect("prepare replacement generation");

        let error = write_codex_rebind_normalized_message(
            &mut old_output,
            &relay_path,
            crate::services::agent_protocol::StreamMessage::Text {
                content: "stale old response".to_string(),
            },
            &mut std::collections::VecDeque::new(),
            &gate,
            old_generation,
        )
        .expect_err("superseded writer must be fenced");
        assert!(error.contains("superseded"));

        let mut replacement_output = std::fs::OpenOptions::new()
            .append(true)
            .open(&relay_path)
            .expect("open replacement output");
        assert!(
            write_codex_rebind_normalized_message(
                &mut replacement_output,
                &relay_path,
                crate::services::agent_protocol::StreamMessage::Text {
                    content: "current response".to_string(),
                },
                &mut std::collections::VecDeque::new(),
                &gate,
                replacement_generation,
            )
            .expect("write replacement generation")
        );
        let relay = std::fs::read_to_string(&relay_path).expect("read relay");
        assert!(!relay.contains("stale old response"));
        assert!(relay.contains("current response"));
    }

    #[test]
    fn codex_rebind_stream_messages_write_normalized_watcher_jsonl() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay_path = tmp.path().join("relay.jsonl");
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(crate::services::agent_protocol::StreamMessage::Text {
                content: "hello".to_string(),
            })
            .expect("send text");
        sender
            .send(crate::services::agent_protocol::StreamMessage::Done {
                result: "hello".to_string(),
                session_id: Some("sess-1".to_string()),
            })
            .expect("send done");
        drop(sender);

        write_codex_rebind_normalized_stream(&relay_path, receiver, String::new(), Vec::new())
            .expect("write relay stream");

        let relay = std::fs::read_to_string(&relay_path).expect("read relay");
        assert!(relay.contains("\"type\":\"assistant\""));
        assert!(relay.contains("\"type\":\"text\""));
        assert!(relay.contains("\"text\":\"hello\""));
        assert!(relay.contains("\"type\":\"result\""));
        assert!(relay.contains("\"session_id\":\"sess-1\""));
    }

    #[test]
    fn codex_rebind_stream_messages_start_new_line_after_torn_append_tail() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay_path = tmp.path().join("relay.jsonl");
        std::fs::write(&relay_path, "{\"type\":\"assistant\"").expect("write torn relay tail");
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(crate::services::agent_protocol::StreamMessage::Text {
                content: "fresh".to_string(),
            })
            .expect("send text");
        drop(sender);

        write_codex_rebind_normalized_stream(&relay_path, receiver, String::new(), Vec::new())
            .expect("write relay stream");

        let relay = std::fs::read_to_string(&relay_path).expect("read relay");
        let lines: Vec<&str> = relay.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "fresh event must not be concatenated onto a torn JSONL tail: {relay}"
        );
        let fresh: serde_json::Value = serde_json::from_str(lines[1]).expect("fresh json line");
        assert_eq!(
            fresh["message"]["content"][0]["text"].as_str(),
            Some("fresh")
        );
    }

    #[test]
    fn codex_rebind_stream_messages_strip_already_relayed_text_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay_path = tmp.path().join("relay.jsonl");
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(crate::services::agent_protocol::StreamMessage::Text {
                content: "hello ".to_string(),
            })
            .expect("send relayed prefix");
        sender
            .send(crate::services::agent_protocol::StreamMessage::Text {
                content: "world".to_string(),
            })
            .expect("send fresh suffix");
        drop(sender);

        write_codex_rebind_normalized_stream(
            &relay_path,
            receiver,
            "hello ".to_string(),
            Vec::new(),
        )
        .expect("write relay stream");

        let relay = std::fs::read_to_string(&relay_path).expect("read relay");
        assert!(!relay.contains("\"text\":\"hello \""));
        assert!(relay.contains("\"text\":\"world\""));
    }

    #[test]
    fn codex_rebind_stream_messages_suppress_replayed_tool_state_before_relayed_text() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay_path = tmp.path().join("relay.jsonl");
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(crate::services::agent_protocol::StreamMessage::ToolUse {
                name: "Bash".to_string(),
                input: r#"{"command":"cargo test"}"#.to_string(),
                tool_use_id: Some("toolu-1".to_string()),
            })
            .expect("send replayed tool use");
        sender
            .send(crate::services::agent_protocol::StreamMessage::ToolResult {
                content: "ok".to_string(),
                is_error: false,
                tool_use_id: Some("toolu-1".to_string()),
            })
            .expect("send replayed tool result");
        sender
            .send(crate::services::agent_protocol::StreamMessage::Text {
                content: "final answer".to_string(),
            })
            .expect("send already relayed final answer");
        sender
            .send(crate::services::agent_protocol::StreamMessage::Done {
                result: "final answer".to_string(),
                session_id: Some("sess-1".to_string()),
            })
            .expect("send done");
        drop(sender);

        write_codex_rebind_normalized_stream(
            &relay_path,
            receiver,
            "final answer".to_string(),
            Vec::new(),
        )
        .expect("write relay stream");

        let relay = std::fs::read_to_string(&relay_path).expect("read relay");
        assert!(
            !relay.contains("\"type\":\"tool_use\""),
            "replayed tool_use before already-relayed text would make watcher append result again: {relay}"
        );
        assert!(
            !relay.contains("\"type\":\"tool_result\""),
            "replayed tool_result before already-relayed text would make watcher append result again: {relay}"
        );
        assert!(!relay.contains("\"text\":\"final answer\""));
        assert!(relay.contains("\"type\":\"result\""));
    }

    #[test]
    fn codex_rebind_stream_messages_preserve_done_suffix_after_relayed_response() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay_path = tmp.path().join("relay.jsonl");
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(crate::services::agent_protocol::StreamMessage::Done {
                result: "already relayed final suffix".to_string(),
                session_id: Some("sess-1".to_string()),
            })
            .expect("send done");
        drop(sender);

        write_codex_rebind_normalized_stream(
            &relay_path,
            receiver,
            "already relayed".to_string(),
            Vec::new(),
        )
        .expect("write relay stream");

        let relay = std::fs::read_to_string(&relay_path).expect("read relay");
        let lines: Vec<serde_json::Value> = relay
            .lines()
            .map(|line| serde_json::from_str(line).expect("json line"))
            .collect();
        assert_eq!(lines.len(), 2, "suffix text must precede result: {relay}");
        assert_eq!(
            lines[0]["message"]["content"][0]["text"].as_str(),
            Some(" final suffix")
        );
        assert_eq!(lines[1]["type"].as_str(), Some("result"));
        assert_eq!(
            lines[1]["result"].as_str(),
            Some("already relayed final suffix")
        );
    }

    #[test]
    fn codex_rebind_stream_messages_skip_existing_normalized_replay_events() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay_path = tmp.path().join("relay.jsonl");
        let existing_text = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "suffix already",
                }]
            }
        });
        let existing_tool = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "id": "toolu-1",
                    "tool_use_id": "toolu-1",
                    "name": "Bash",
                    "input": {"command": "cargo test"},
                }]
            }
        });
        std::fs::write(&relay_path, format!("{existing_text}\n{existing_tool}\n"))
            .expect("write existing relay");

        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(crate::services::agent_protocol::StreamMessage::Text {
                content: "suffix already".to_string(),
            })
            .expect("send replayed suffix");
        sender
            .send(crate::services::agent_protocol::StreamMessage::ToolUse {
                name: "Bash".to_string(),
                input: r#"{"command":"cargo test"}"#.to_string(),
                tool_use_id: Some("toolu-1".to_string()),
            })
            .expect("send replayed tool use");
        sender
            .send(crate::services::agent_protocol::StreamMessage::Text {
                content: "fresh tail".to_string(),
            })
            .expect("send fresh tail");
        drop(sender);

        write_codex_rebind_normalized_stream(
            &relay_path,
            receiver,
            String::new(),
            vec![existing_text, existing_tool],
        )
        .expect("write relay stream");

        let relay = std::fs::read_to_string(&relay_path).expect("read relay");
        assert_eq!(
            relay.matches("\"text\":\"suffix already\"").count(),
            1,
            "existing normalized text event must not be appended twice: {relay}"
        );
        assert_eq!(
            relay.matches("\"type\":\"tool_use\"").count(),
            1,
            "existing normalized tool event must not be appended twice: {relay}"
        );
        assert!(relay.contains("\"text\":\"fresh tail\""));
    }

    #[test]
    fn codex_rebind_stream_messages_skip_existing_normalized_replay_events_after_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay_path = tmp.path().join("relay.jsonl");
        let existing_prefix = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "old prefix",
                }]
            }
        });
        let existing_text = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "suffix already",
                }]
            }
        });
        let existing_tool = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "id": "toolu-1",
                    "tool_use_id": "toolu-1",
                    "name": "Bash",
                    "input": {"command": "cargo test"},
                }]
            }
        });
        std::fs::write(
            &relay_path,
            format!("{existing_prefix}\n{existing_text}\n{existing_tool}\n"),
        )
        .expect("write existing relay");

        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(crate::services::agent_protocol::StreamMessage::Text {
                content: "suffix already".to_string(),
            })
            .expect("send replayed suffix");
        sender
            .send(crate::services::agent_protocol::StreamMessage::ToolUse {
                name: "Bash".to_string(),
                input: r#"{"command":"cargo test"}"#.to_string(),
                tool_use_id: Some("toolu-1".to_string()),
            })
            .expect("send replayed tool use");
        sender
            .send(crate::services::agent_protocol::StreamMessage::Text {
                content: "fresh tail".to_string(),
            })
            .expect("send fresh tail");
        drop(sender);

        write_codex_rebind_normalized_stream(
            &relay_path,
            receiver,
            String::new(),
            vec![existing_prefix, existing_text, existing_tool],
        )
        .expect("write relay stream");

        let relay = std::fs::read_to_string(&relay_path).expect("read relay");
        assert_eq!(
            relay.matches("\"text\":\"old prefix\"").count(),
            1,
            "existing normalized prefix event must remain single: {relay}"
        );
        assert_eq!(
            relay.matches("\"text\":\"suffix already\"").count(),
            1,
            "existing normalized text event must not be appended twice: {relay}"
        );
        assert_eq!(
            relay.matches("\"type\":\"tool_use\"").count(),
            1,
            "existing normalized tool event must not be appended twice: {relay}"
        );
        assert!(relay.contains("\"text\":\"fresh tail\""));
    }

    #[test]
    fn codex_rebind_stream_messages_preserve_tool_use_input_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay_path = tmp.path().join("relay.jsonl");
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(crate::services::agent_protocol::StreamMessage::ToolUse {
                name: "Bash".to_string(),
                input: r#"{"command":"cargo test"}"#.to_string(),
                tool_use_id: Some("toolu-1".to_string()),
            })
            .expect("send tool use");
        drop(sender);

        write_codex_rebind_normalized_stream(&relay_path, receiver, String::new(), Vec::new())
            .expect("write relay stream");

        let relay = std::fs::read_to_string(&relay_path).expect("read relay");
        let line: serde_json::Value = serde_json::from_str(relay.trim()).expect("json line");
        let input = &line["message"]["content"][0]["input"];
        assert!(
            input.is_object(),
            "tool input must stay as JSON object, got {input:?}"
        );
        assert_eq!(input["command"].as_str(), Some("cargo test"));
    }
}
