//! Structured launch acknowledgments and exact completion metadata shared with the footer.
use crate::services::agent_protocol::SubagentSummary;
use serde_json::Value;

pub(crate) fn summary_from_tool_use_result(
    value: &Value,
) -> Option<(SubagentSummary, Option<String>)> {
    let result = value.get("toolUseResult")?;
    // `toolUseResult` may be a string (non-subagent tools) — only objects carry
    // the subagent accounting.
    let result = result.as_object()?;

    if tool_use_result_is_async_launch(value) {
        return None;
    }

    let agent_id = agent_id_from_tool_use_result(value);

    let has_accounting_field = result.contains_key("totalToolUseCount")
        || result.contains_key("totalTokens")
        || result.contains_key("totalDurationMs");

    let tool_count = result.get("totalToolUseCount").and_then(as_u64_lenient);
    let tokens = result.get("totalTokens").and_then(as_u64_lenient);
    // Round partial seconds up so a sub-second subagent still reads `1s` rather
    // than `0s`; `0ms` stays `0s`.
    let duration_secs = result
        .get("totalDurationMs")
        .and_then(as_u64_lenient)
        .map(|ms| ms.div_ceil(1000));

    let summary = SubagentSummary {
        tool_count,
        tokens,
        duration_secs,
    };

    // An agent id alone is a launch acknowledgment, so require completion accounting.
    if !has_accounting_field && summary.is_empty() {
        return None;
    }
    Some((summary, agent_id))
}

pub(crate) fn agent_id_from_tool_use_result(value: &Value) -> Option<String> {
    value
        .get("toolUseResult")?
        .as_object()?
        .get("agentId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

pub(crate) fn tool_use_result_is_async_launch(value: &Value) -> bool {
    value
        .get("toolUseResult")
        .and_then(Value::as_object)
        .is_some_and(|result| {
            result.get("isAsync").and_then(Value::as_bool) == Some(true)
                || result.get("status").and_then(Value::as_str) == Some("async_launched")
        })
}

fn as_u64_lenient(value: &Value) -> Option<u64> {
    if let Some(n) = value.as_u64() {
        return Some(n);
    }
    if let Some(f) = value.as_f64() {
        if f.is_finite() && f >= 0.0 {
            return Some(f as u64);
        }
    }
    value.as_str().and_then(|s| s.trim().parse::<u64>().ok())
}
