//! Render messages decoded by the canonical Codex rollout parser through the
//! watcher's existing normalized-event path. Raw byte offsets stay in the
//! outer reader; normalized render bytes never become source coordinates.
use super::*;

pub(in crate::services::discord) fn watcher_source_witness(
    provider: &ProviderKind,
    session: &str,
    path: &str,
) -> Option<crate::services::cluster::stream_relay::SourceWitness> {
    use crate::services::discord::delivery_lease_cell::source_epoch_observer as observer;
    let native = *provider == ProviderKind::Codex
        && crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session)
            .is_some_and(|binding| {
                binding.runtime_kind
                    == crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
                    && binding.output_path == path
            });
    if native {
        Some(observer::read_source_epoch_witness(session))
    } else {
        observer::marker_if_enabled(session)
    }
}

pub(in crate::services::discord) fn is_native_codex_payload(
    provider: &ProviderKind,
    payload: &str,
) -> bool {
    *provider == ProviderKind::Codex
        && payload.lines().any(|line| {
            serde_json::from_str(line).ok().is_some_and(|value| {
                crate::services::codex_tui::rollout_tail::RolloutRecordDecoder::is_native_record(
                    &value,
                )
            })
        })
}

pub(in crate::services::discord) fn read_native_codex_state(
    path: &str,
    start: u64,
    end: u64,
    expected_file: crate::services::cluster::stream_relay::SourceFileIdentity,
    session: &str,
    generation: i64,
    expected_stamp: Option<crate::services::cluster::stream_relay::SourceStamp>,
) -> Result<crate::services::codex_tui::rollout_tail::RolloutRecordDecoder, String> {
    use crate::services::cluster::stream_relay::SourceFileIdentity;
    use std::io::{BufReader, Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if expected_file == SourceFileIdentity::Unavailable
        || SourceFileIdentity::from_open_file(&file) != expected_file
        || generation == 0
        || read_generation_file_mtime_ns(session) != generation
        || end < start
        || end > file.metadata().map_err(|e| e.to_string())?.len()
    {
        return Err("native Codex restore source identity or range changed".into());
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    // Stream only this turn's captured prefix; never allocate the whole rollout.
    let mut range = file.take(end - start);
    let decoder = crate::services::codex_tui::rollout_tail::RolloutRecordDecoder::from_reader(
        BufReader::new(&mut range),
    )?;
    let current_file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let current_identity = SourceFileIdentity::from_open_file(&current_file);
    let stamp_matches = expected_stamp.is_none_or(|expected| {
        use crate::services::discord::delivery_lease_cell::source_epoch_observer as observer;
        observer::source_stamp(
            session,
            observer::read_source_epoch_witness(session),
            current_identity,
        ) == Some(expected)
    });
    if range.limit() != 0
        || read_generation_file_mtime_ns(session) != generation
        || current_identity != expected_file
        || !stamp_matches
    {
        return Err("native Codex restore source changed while reading".into());
    }
    Ok(decoder)
}
use crate::services::agent_protocol::StreamMessage;

pub(super) fn process_native_codex_messages(
    messages: Vec<StreamMessage>,
    state: &mut StreamLineState,
    response: &mut String,
    tools: &mut WatcherToolState,
) -> WatcherLineOutcome {
    let mut result = WatcherLineOutcome::default();
    for message in messages {
        let mut canonical_terminal_response = None;
        let value = match message {
            StreamMessage::Init { session_id, .. } => {
                state.last_session_id = Some(session_id);
                continue;
            }
            StreamMessage::Text { content } => serde_json::json!({
                "type": "content_block_delta", "delta": {"text": content}
            }),
            StreamMessage::ToolUse {
                name,
                input,
                tool_use_id,
            } => serde_json::json!({
                "type": "assistant", "message": {"content": [{
                    "type": "tool_use", "name": name, "id": tool_use_id,
                    "input": serde_json::from_str::<serde_json::Value>(&input)
                        .unwrap_or(serde_json::Value::String(input))
                }]}
            }),
            StreamMessage::ToolResult {
                content,
                is_error,
                tool_use_id,
            } => serde_json::json!({
                "type": "user", "message": {"content": [{
                    "type": "tool_result", "content": content,
                    "is_error": is_error, "tool_use_id": tool_use_id
                }]}
            }),
            StreamMessage::Thinking { .. } => serde_json::json!({
                "type": "assistant", "message": {"content": [{"type": "thinking"}]}
            }),
            StreamMessage::StatusUpdate {
                model,
                input_tokens,
                cache_create_tokens,
                cache_read_tokens,
                output_tokens,
                ..
            } => {
                if model.is_some() {
                    state.last_model = model;
                }
                if let Some(tokens) = input_tokens {
                    state.accum_input_tokens = tokens;
                }
                if let Some(tokens) = cache_create_tokens {
                    state.accum_cache_create_tokens = tokens;
                }
                if let Some(tokens) = cache_read_tokens {
                    state.accum_cache_read_tokens = tokens;
                }
                if let Some(tokens) = output_tokens {
                    state.accum_output_tokens = tokens;
                }
                continue;
            }
            StreamMessage::Done { result, session_id } => {
                canonical_terminal_response = Some(result.clone());
                serde_json::json!({"type": "result", "result": result, "session_id": session_id})
            }
            _ => continue,
        };
        let mut normalized = format!("{value}\n");
        let outcome = process_watcher_lines(&mut normalized, state, response, tools);
        // Wrapper bookkeeping may append a tool-only multiline result. The
        // native decoder already assembled the entire canonical response.
        if let Some(canonical) = canonical_terminal_response {
            *response = canonical;
        }
        result.assistant_text_seen |= outcome.assistant_text_seen;
        if outcome.found_result {
            result.found_result = true;
            result.terminal_kind = outcome.terminal_kind;
        }
    }
    result
}
