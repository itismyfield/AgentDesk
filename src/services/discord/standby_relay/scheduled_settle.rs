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
    result_text: &str,
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
    // Without this turn's terminal result (a drain fallback body) nothing is settled.
    let Some(is_error) = state
        .turn_start_offset
        .and_then(|start| terminal_result_is_error(output_path, start))
    else {
        return;
    };
    let outcome = if is_error {
        Some(crate::services::scheduled_messages::turn_completion::ScheduledTurnOutcome::ProviderError)
    } else {
        crate::services::scheduled_messages::turn_completion::classify(result_text, false, false)
    };
    crate::services::discord::scheduled_turn::settle(shared.pg_pool.as_ref(), &state, outcome)
        .await;
}

/// Whether the turn's first `result` event at or after `start` is a typed error;
/// `None` when the turn has no result event yet.
pub(super) fn terminal_result_is_error(output_path: &str, start: u64) -> Option<bool> {
    let bytes = std::fs::read(output_path).ok()?;
    let start = usize::try_from(start)
        .ok()
        .filter(|start| *start <= bytes.len())?;
    String::from_utf8_lossy(&bytes[start..])
        .lines()
        .find_map(|line| {
            let parsed = serde_json::from_str::<Value>(line.trim()).ok()?;
            (parsed.get("type").and_then(Value::as_str) == Some("result"))
                .then(|| parsed.get("is_error").and_then(Value::as_bool) == Some(true))
        })
}

#[cfg(test)]
#[path = "scheduled_settle_tests.rs"]
mod tests;
