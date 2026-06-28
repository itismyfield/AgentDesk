use serde_json::Value;
use std::collections::HashSet;

use crate::services::agent_protocol::StreamMessage;

use super::RelaySuppressionSender;

#[derive(Debug, Default)]
pub(super) struct RolloutParseState {
    pub(super) session_id: Option<String>,
    pub(super) final_text: String,
    pub(super) saw_assistant_text: bool,
    pub(super) lines_read: usize,
    pub(super) bytes_read: u64,
    pub(super) pending_tool_calls: HashSet<String>,
    pub(super) pending_tool_calls_unkeyed: usize,
    pub(super) lifecycle_activity: bool,
    pub(super) turn_complete_seen: bool,
    pub(super) task_complete_fallback_text: Option<String>,
    pub(super) seen_any_event_msg: bool,
    pub(super) composer_ready_seen: bool,
    pub(super) explicit_composer_ready_seen: bool,
    pub(super) synthetic_composer_ready_seen: bool,
    pub(super) hook_completion_seen: bool,
    pub(super) explicit_completion_missing_text_warned: bool,
    pub(super) agent_message_item_completed_seen: bool,
    pub(super) tmux_session_name: Option<String>,
    pub(super) discord_origin_prompt: Option<String>,
    pub(super) heuristic_finalize_waiting_for_completion_logged: bool,
    pub(super) last_emitted_text_ended_with_newline: Option<bool>,
}

impl RolloutParseState {
    pub(super) fn record(&mut self, line_len: usize) {
        self.lines_read += 1;
        self.bytes_read += line_len as u64;
    }

    pub(super) fn has_pending_tool_call(&self) -> bool {
        !self.pending_tool_calls.is_empty() || self.pending_tool_calls_unkeyed > 0
    }

    pub(super) fn push_message_text(&mut self, text: &str) -> String {
        let chunk = join_streamed_message_boundary(self.last_emitted_text_ended_with_newline, text);
        self.final_text.push_str(&chunk);
        self.last_emitted_text_ended_with_newline = Some(text.ends_with('\n'));
        chunk
    }
}

pub(super) fn process_rollout_line_bytes(
    line: &[u8],
    sender: &RelaySuppressionSender<'_>,
    state: &mut RolloutParseState,
) -> bool {
    let Ok(line) = std::str::from_utf8(line) else {
        tracing::debug!("ignoring non-UTF-8 Codex rollout line");
        return false;
    };
    process_rollout_line(line, sender, state)
}

fn process_rollout_line(
    line: &str,
    sender: &RelaySuppressionSender<'_>,
    state: &mut RolloutParseState,
) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Ok(json) = serde_json::from_str::<Value>(trimmed) else {
        tracing::debug!("ignoring malformed Codex rollout line");
        return false;
    };

    state.lifecycle_activity = false;
    let messages = rollout_messages(&json, state);
    observe_rollout_user_prompt(&json, state);
    maybe_observe_synthetic_composer_ready(state);
    let emitted = !messages.is_empty();
    for message in messages {
        sender.send(message);
    }
    let activity = emitted || state.lifecycle_activity;
    state.lifecycle_activity = false;
    activity
}

fn rollout_messages(json: &Value, state: &mut RolloutParseState) -> Vec<StreamMessage> {
    match json.get("type").and_then(Value::as_str).unwrap_or("") {
        "session_meta" => session_meta_message(json, state).into_iter().collect(),
        "response_item" => response_item_messages(json, state),
        "event_msg" => event_msg_message(json, state).into_iter().collect(),
        "item.completed" => item_completed_message(json, state).into_iter().collect(),
        "turn.completed" => {
            state.turn_complete_seen = true;
            Vec::new()
        }
        _ => Vec::new(),
    }
}

pub(super) fn observe_rollout_user_prompt(json: &Value, state: &mut RolloutParseState) {
    let Some(tmux_session_name) = state.tmux_session_name.clone() else {
        return;
    };
    let Some((prompt, entry_id)) =
        crate::services::tui_prompt_dedupe::extract_codex_rollout_user_prompt_with_entry_id(json)
    else {
        return;
    };
    if state
        .discord_origin_prompt
        .as_deref()
        .is_some_and(|expected| {
            crate::services::tui_prompt_dedupe::prompts_match(expected, &prompt)
        })
    {
        crate::services::tui_prompt_dedupe::record_suppressed_discord_origin_prompt(
            "codex",
            &tmux_session_name,
            &prompt,
        );
        state.discord_origin_prompt = None;
        tracing::debug!(
            tmux_session_name,
            "suppressed Codex launch prompt observed in rollout"
        );
        return;
    }
    let observation = crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_entry_id_at(
        "codex",
        &tmux_session_name,
        &prompt,
        entry_id.as_deref(),
        chrono::Utc::now(),
    );
    tracing::debug!(
        tmux_session_name,
        observation = ?observation,
        entry_id = entry_id.as_deref().unwrap_or(""),
        "observed Codex rollout user prompt"
    );
}

fn session_meta_message(json: &Value, state: &mut RolloutParseState) -> Option<StreamMessage> {
    let session_id = json
        .get("payload")
        .and_then(|payload| payload.get("id"))
        .and_then(Value::as_str)?
        .trim();
    if session_id.is_empty() {
        return None;
    }
    state.session_id = Some(session_id.to_string());
    Some(StreamMessage::Init {
        session_id: session_id.to_string(),
        raw_session_id: None,
    })
}

fn response_item_messages(json: &Value, state: &mut RolloutParseState) -> Vec<StreamMessage> {
    let Some(payload) = json.get("payload") else {
        return Vec::new();
    };
    match payload.get("type").and_then(Value::as_str).unwrap_or("") {
        "message" => response_message_items(payload, state),
        "function_call" | "custom_tool_call" | "tool_search_call" => {
            match payload.get("call_id").and_then(Value::as_str) {
                Some(id) if !id.is_empty() => {
                    state.pending_tool_calls.insert(id.to_string());
                }
                _ => {
                    state.pending_tool_calls_unkeyed =
                        state.pending_tool_calls_unkeyed.saturating_add(1);
                }
            }
            state.lifecycle_activity = true;
            tool_call_message(payload).into_iter().collect()
        }
        "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
            match payload.get("call_id").and_then(Value::as_str) {
                Some(id) if !id.is_empty() => {
                    state.pending_tool_calls.remove(id);
                }
                _ => {
                    state.pending_tool_calls_unkeyed =
                        state.pending_tool_calls_unkeyed.saturating_sub(1);
                }
            }
            state.lifecycle_activity = true;
            tool_result_message(payload).into_iter().collect()
        }
        "reasoning" => {
            state.lifecycle_activity = true;
            vec![StreamMessage::redacted_thinking()]
        }
        _ => Vec::new(),
    }
}

pub(super) fn join_streamed_message_boundary(
    prev_ended_with_newline: Option<bool>,
    text: &str,
) -> String {
    match prev_ended_with_newline {
        None => text.to_string(),
        Some(true) => text.to_string(),
        Some(false) if text.starts_with('\n') => text.to_string(),
        Some(false) => format!("\n\n{text}"),
    }
}

fn response_message_items(payload: &Value, state: &mut RolloutParseState) -> Vec<StreamMessage> {
    if payload.get("role").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    let Some(content) = payload.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let commentary_phase = payload.get("phase").and_then(Value::as_str) == Some("commentary");
    content
        .iter()
        .filter_map(|item| {
            let item_type = item.get("type").and_then(Value::as_str)?;
            if item_type != "output_text" && item_type != "text" {
                return None;
            }
            let text = item.get("text").and_then(Value::as_str)?.to_string();
            if text.is_empty() {
                return None;
            }
            let emitted = state.push_message_text(&text);
            if !commentary_phase {
                state.saw_assistant_text = true;
            } else {
                state.lifecycle_activity = true;
            }
            Some(StreamMessage::Text { content: emitted })
        })
        .collect()
}

fn item_completed_message(json: &Value, state: &mut RolloutParseState) -> Option<StreamMessage> {
    let item = json.get("item")?;
    if item.get("type").and_then(Value::as_str) == Some("agent_message") {
        state.agent_message_item_completed_seen = true;
        state.lifecycle_activity = true;
    }
    None
}

pub(super) fn tool_call_message(payload: &Value) -> Option<StreamMessage> {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())?;
    let input = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .or_else(|| payload.get("action"))
        .map(compact_json_or_string)
        .unwrap_or_else(|| "{}".to_string());
    let tool_use_id = payload
        .get("call_id")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(StreamMessage::ToolUse {
        name: name.to_string(),
        input,
        tool_use_id,
    })
}

pub(super) fn tool_result_message(payload: &Value) -> Option<StreamMessage> {
    let content = payload
        .get("output")
        .or_else(|| payload.get("content"))
        .map(compact_json_or_string)?;
    if content.is_empty() {
        return None;
    }
    let tool_use_id = payload
        .get("call_id")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(StreamMessage::ToolResult {
        content,
        is_error: payload
            .get("is_error")
            .or_else(|| payload.get("isError"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        tool_use_id,
    })
}

fn event_msg_message(json: &Value, state: &mut RolloutParseState) -> Option<StreamMessage> {
    let payload = json.get("payload")?;
    let synthetic = payload
        .get("synthetic")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !synthetic {
        state.seen_any_event_msg = true;
    }
    match payload.get("type").and_then(Value::as_str)? {
        "token_count" => token_count_status(payload),
        "agent_reasoning" => Some(StreamMessage::redacted_thinking()),
        "composer_ready" => {
            state.composer_ready_seen = true;
            if synthetic {
                state.synthetic_composer_ready_seen = true;
            } else {
                state.explicit_composer_ready_seen = true;
            }
            state.lifecycle_activity = true;
            if let Some(tmux_session_name) = state.tmux_session_name.as_deref() {
                crate::services::codex_tui::input::record_rollout_composer_ready(tmux_session_name);
            }
            None
        }
        "task_complete" => {
            state.turn_complete_seen = true;
            if state.task_complete_fallback_text.is_none() {
                state.task_complete_fallback_text = payload
                    .get("last_agent_message")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned);
            }
            None
        }
        _ => {
            state.lifecycle_activity = true;
            None
        }
    }
}

fn maybe_observe_synthetic_composer_ready(state: &mut RolloutParseState) {
    if state.composer_ready_seen || state.has_pending_tool_call() {
        return;
    }
    if !state.turn_complete_seen && !state.agent_message_item_completed_seen {
        return;
    }
    let synthetic = serde_json::json!({
        "type": "event_msg",
        "payload": {
            "type": "composer_ready",
            "synthetic": true,
        },
    });
    let _ = event_msg_message(&synthetic, state);
}

pub(super) fn token_count_status(payload: &Value) -> Option<StreamMessage> {
    let info = payload.get("info")?;
    let last_usage = info.get("last_token_usage");
    let total_usage = info.get("total_token_usage");
    let output_usage = last_usage.or(total_usage);
    let (input_tokens, cache_read_tokens) = match last_usage {
        Some(usage) => {
            let total_input = usage.get("input_tokens").and_then(Value::as_u64);
            let cached_input = usage
                .get("cached_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            (
                total_input.map(|tokens| tokens.saturating_sub(cached_input)),
                (cached_input > 0).then_some(cached_input),
            )
        }
        None => (None, None),
    };
    let output_tokens = output_usage
        .and_then(|usage| usage.get("output_tokens"))
        .and_then(Value::as_u64);
    if input_tokens.is_none() && cache_read_tokens.is_none() && output_tokens.is_none() {
        return None;
    }
    Some(StreamMessage::StatusUpdate {
        model: None,
        cost_usd: None,
        total_cost_usd: None,
        duration_ms: None,
        num_turns: None,
        input_tokens,
        cache_create_tokens: None,
        cache_read_tokens,
        output_tokens,
    })
}

fn compact_json_or_string(value: &Value) -> String {
    value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| serde_json::to_string(value).unwrap_or_default())
}
