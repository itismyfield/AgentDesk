//! Render messages decoded by the canonical Codex rollout parser through the
//! watcher's existing normalized-event path. Raw byte offsets stay in the
//! outer reader; normalized render bytes never become source coordinates.
use super::*;
use crate::services::agent_protocol::StreamMessage;

pub(super) fn process_native_codex_messages(
    messages: Vec<StreamMessage>,
    state: &mut StreamLineState,
    response: &mut String,
    tools: &mut WatcherToolState,
) -> WatcherLineOutcome {
    let mut result = WatcherLineOutcome::default();
    for message in messages {
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
                // Native fallback can supersede commentary; preserve its exact
                // canonical body instead of applying the wrapper append policy.
                *response = result.clone();
                serde_json::json!({"type": "result", "result": result, "session_id": session_id})
            }
            _ => continue,
        };
        let mut normalized = format!("{value}\n");
        let outcome = process_watcher_lines(&mut normalized, state, response, tools);
        result.assistant_text_seen |= outcome.assistant_text_seen;
        if outcome.found_result {
            result.found_result = true;
            result.terminal_kind = outcome.terminal_kind;
        }
    }
    result
}
