//! Settles a standby-delivered scheduled turn before its inflight row is cleared;
//! only this exact binding's row settles, and an `is_error` result is never delivered.

use super::*;

pub(super) async fn settle(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    output_path: &str,
    placeholder_msg_id: Option<MessageId>,
    binding: &StandbyRelayTurnBinding,
    (result_text, result_offset): (&str, Option<u64>),
) {
    let Some(state) = super::super::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return;
    };
    if !state.scheduled.may_settle()
        || !standby_inflight_matches(&state, output_path, placeholder_msg_id)
        || !binding.identity.matches_state(&state)
    {
        return;
    }
    let is_error = result_offset.is_some_and(|offset| result_line_is_error(output_path, offset));
    let outcome = crate::services::scheduled_messages::turn_completion::classify(
        result_text,
        is_error,
        false,
    );
    crate::services::discord::scheduled_turn::settle(shared.pg_pool.as_ref(), &state, outcome)
        .await;
}

/// Whether the result event starting at `offset` is a typed error result.
pub(super) fn result_line_is_error(output_path: &str, offset: u64) -> bool {
    let Ok(bytes) = std::fs::read(output_path) else {
        return false;
    };
    let Some(start) = usize::try_from(offset)
        .ok()
        .filter(|start| *start < bytes.len())
    else {
        return false;
    };
    let line = bytes[start..]
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    serde_json::from_slice::<Value>(line).is_ok_and(|parsed| {
        parsed.get("type").and_then(Value::as_str) == Some("result")
            && parsed.get("is_error").and_then(Value::as_bool) == Some(true)
    })
}

#[cfg(test)]
#[path = "scheduled_settle_tests.rs"]
mod tests;
