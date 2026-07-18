use serde_json::Value;
use std::sync::mpsc::Sender;

use crate::services::agent_protocol::StreamMessage;

pub(super) struct AssistantUsageState<'a> {
    pub(super) last_model: &'a mut Option<String>,
    pub(super) last_call_input_tokens: &'a mut u64,
    pub(super) last_call_cache_create_tokens: &'a mut u64,
    pub(super) last_call_cache_read_tokens: &'a mut u64,
    pub(super) cumulative_output_tokens: &'a mut u64,
    pub(super) saw_per_message_usage: &'a mut bool,
}

pub(super) fn observe_assistant_usage(
    msg_obj: &Value,
    sender: &Sender<StreamMessage>,
    state: AssistantUsageState<'_>,
) -> bool {
    if let Some(model) = msg_obj.get("model").and_then(Value::as_str) {
        *state.last_model = Some(model.to_string());
    }
    let Some(usage) = msg_obj.get("usage") else {
        return true;
    };
    *state.saw_per_message_usage = true;
    let input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
    let cache_read_tokens = usage.get("cache_read_input_tokens").and_then(Value::as_u64);
    let cache_create_tokens = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    *state.last_call_input_tokens = input_tokens.unwrap_or(0);
    *state.last_call_cache_read_tokens = cache_read_tokens.unwrap_or(0);
    *state.last_call_cache_create_tokens = cache_create_tokens.unwrap_or(0);
    if let Some(output_tokens) = usage.get("output_tokens").and_then(Value::as_u64) {
        *state.cumulative_output_tokens =
            state.cumulative_output_tokens.saturating_add(output_tokens);
    }
    let (Some(input_tokens), Some(cache_create_tokens), Some(cache_read_tokens)) =
        (input_tokens, cache_create_tokens, cache_read_tokens)
    else {
        return true;
    };
    sender
        .send(StreamMessage::ActiveUsageSnapshot {
            model: msg_obj
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            input_tokens,
            cache_create_tokens,
            cache_read_tokens,
        })
        .is_ok()
}
