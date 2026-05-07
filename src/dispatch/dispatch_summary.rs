use serde_json::Value;

const MAX_DISPATCH_SUMMARY_CHARS: usize = 160;

fn normalize_dispatch_summary_text(value: &str) -> Option<String> {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }

    let mut summary = String::new();
    for (index, ch) in normalized.chars().enumerate() {
        if index >= MAX_DISPATCH_SUMMARY_CHARS {
            summary.push_str("...");
            break;
        }
        summary.push(ch);
    }
    Some(summary)
}

pub(crate) fn parse_dispatch_json_text(raw: Option<&str>) -> Option<Value> {
    raw.and_then(|text| serde_json::from_str::<Value>(text).ok())
}

fn top_level_string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|entry| entry.as_str())
        .and_then(normalize_dispatch_summary_text)
}

fn first_string_field(values: &[Option<&Value>], key: &str) -> Option<String> {
    values
        .iter()
        .flatten()
        .find_map(|value| top_level_string_field(value, key))
}

fn first_bool_field(values: &[Option<&Value>], key: &str) -> Option<bool> {
    values
        .iter()
        .flatten()
        .find_map(|value| value.get(key).and_then(|entry| entry.as_bool()))
}

fn extract_summary_like_text(value: &Value) -> Option<String> {
    const SUMMARY_KEYS: &[&str] = &[
        "summary",
        "work_summary",
        "result_summary",
        "task_summary",
        "completion_summary",
        "message",
        "final_message",
    ];

    match value {
        Value::String(text) => normalize_dispatch_summary_text(text),
        Value::Object(map) => {
            for key in SUMMARY_KEYS {
                if let Some(summary) = map.get(*key).and_then(extract_summary_like_text) {
                    return Some(summary);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(extract_summary_like_text),
        _ => None,
    }
}

fn extract_fallback_text(value: &Value) -> Option<String> {
    const FALLBACK_KEYS: &[&str] = &["notes", "comment", "content"];

    match value {
        Value::String(text) => normalize_dispatch_summary_text(text),
        Value::Object(map) => {
            for key in FALLBACK_KEYS {
                if let Some(summary) = map.get(*key).and_then(extract_fallback_text) {
                    return Some(summary);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(extract_fallback_text),
        _ => None,
    }
}

fn humanize_dispatch_code(value: &str) -> Option<String> {
    let normalized = normalize_dispatch_summary_text(value)?;
    match normalized.as_str() {
        "auto_cancelled_on_terminal_card" | "js_terminal_cleanup" => {
            Some("terminal card cleanup".to_string())
        }
        "superseded_by_dispute_re_review" => Some("superseded by dispute re-review".to_string()),
        "invalid_dispute_rereview_target" => Some("invalid dispute re-review target".to_string()),
        "startup_reconcile_duplicate_review" => Some("duplicate review cleanup".to_string()),
        "orphan_recovery" => Some("recovered orphan dispatch".to_string()),
        "orphan_recovery_rollback" => Some("orphan recovery rollback".to_string()),
        _ if normalized.contains(' ') => Some(normalized),
        _ => Some(normalized.replace(['_', '-'], " ")),
    }
}

fn summarize_noop(values: &[Option<&Value>]) -> Option<String> {
    let is_noop = values.iter().flatten().any(|value| {
        value
            .get("work_outcome")
            .and_then(|entry| entry.as_str())
            .is_some_and(|entry| entry == "noop")
            || value
                .get("completed_without_changes")
                .and_then(|entry| entry.as_bool())
                == Some(true)
    });
    if !is_noop {
        return None;
    }

    let detail = first_string_field(values, "noop_reason")
        .or_else(|| first_string_field(values, "notes"))
        .or_else(|| first_string_field(values, "comment"));
    Some(match detail {
        Some(detail) => format!("No-op: {detail}"),
        None => "No-op".to_string(),
    })
}

fn summarize_decision(dispatch_type: Option<&str>, values: &[Option<&Value>]) -> Option<String> {
    let decision = first_string_field(values, "decision")?;
    let base = match (dispatch_type, decision.as_str()) {
        (Some("review-decision"), "accept") => "Accepted review feedback".to_string(),
        (Some("review-decision"), "dispute") => "Disputed review feedback".to_string(),
        (Some("review-decision"), "dismiss") => "Dismissed review feedback".to_string(),
        (_, "rework") => "Rework requested".to_string(),
        _ => {
            let label = humanize_dispatch_code(&decision).unwrap_or(decision);
            format!("Decision: {label}")
        }
    };

    let comment = first_string_field(values, "comment");
    Some(match comment {
        Some(comment) => format!("{base}: {comment}"),
        None => base,
    })
}

fn summarize_cancellation(values: &[Option<&Value>]) -> Option<String> {
    let reason = first_string_field(values, "reason")
        .and_then(|reason| humanize_dispatch_code(&reason).or(Some(reason)));
    let completion_source = first_string_field(values, "completion_source")
        .and_then(|source| humanize_dispatch_code(&source).or(Some(source)));
    let detail = reason.or(completion_source)?;
    Some(format!("Cancelled: {detail}"))
}

fn summarize_orphan(values: &[Option<&Value>]) -> Option<String> {
    if first_bool_field(values, "orphan_failed") == Some(true) {
        return Some("Orphan recovery rollback".to_string());
    }
    if first_bool_field(values, "auto_completed") == Some(true)
        && first_string_field(values, "completion_source").as_deref() == Some("orphan_recovery")
    {
        return Some("Recovered orphan dispatch".to_string());
    }
    None
}

fn summarize_rework_context(values: &[Option<&Value>]) -> Option<String> {
    if let Some(comment) = first_string_field(values, "comment")
        && first_string_field(values, "pm_decision").as_deref() == Some("rework")
    {
        return Some(format!("PM requested rework: {comment}"));
    }

    if let Some(resumed_from) = first_string_field(values, "resumed_from") {
        let detail = humanize_dispatch_code(&resumed_from).unwrap_or(resumed_from);
        return Some(format!("Resumed from {detail}"));
    }

    if first_bool_field(values, "resume") == Some(true) {
        return Some("Resumed rework".to_string());
    }

    None
}

fn summarize_verdict(values: &[Option<&Value>]) -> Option<String> {
    let verdict = first_string_field(values, "verdict")?;
    let detail = humanize_dispatch_code(&verdict).unwrap_or(verdict);
    Some(format!("Review verdict: {detail}"))
}

pub(crate) fn summarize_dispatch_result(
    dispatch_type: Option<&str>,
    status: Option<&str>,
    result: Option<&Value>,
    context: Option<&Value>,
) -> Option<String> {
    let values = [result, context];

    result
        .and_then(extract_summary_like_text)
        .or_else(|| context.and_then(extract_summary_like_text))
        .or_else(|| summarize_noop(&values))
        .or_else(|| summarize_decision(dispatch_type, &values))
        .or_else(|| summarize_orphan(&values))
        .or_else(|| {
            if status == Some("cancelled") {
                summarize_cancellation(&values)
            } else {
                None
            }
        })
        .or_else(|| {
            if dispatch_type == Some("rework") {
                summarize_rework_context(&values)
            } else {
                None
            }
        })
        .or_else(|| summarize_verdict(&values))
        .or_else(|| result.and_then(extract_fallback_text))
        .or_else(|| context.and_then(extract_fallback_text))
}

pub(crate) fn summarize_dispatch_from_text(
    dispatch_type: Option<&str>,
    status: Option<&str>,
    result_raw: Option<&str>,
    context_raw: Option<&str>,
) -> Option<String> {
    let result = parse_dispatch_json_text(result_raw);
    let context = parse_dispatch_json_text(context_raw);
    summarize_dispatch_result(dispatch_type, status, result.as_ref(), context.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarize_dispatch_result_handles_cancel_reason() {
        let summary = summarize_dispatch_result(
            Some("implementation"),
            Some("cancelled"),
            Some(&json!({
                "reason": "auto_cancelled_on_terminal_card"
            })),
            None,
        );

        assert_eq!(summary.as_deref(), Some("Cancelled: terminal card cleanup"));
    }

    #[test]
    fn summarize_dispatch_result_handles_review_decision_comment() {
        let summary = summarize_dispatch_result(
            Some("review-decision"),
            Some("completed"),
            Some(&json!({
                "decision": "accept",
                "comment": "Looks good"
            })),
            None,
        );

        assert_eq!(
            summary.as_deref(),
            Some("Accepted review feedback: Looks good")
        );
    }

    #[test]
    fn summarize_dispatch_result_handles_rework_context() {
        let summary = summarize_dispatch_result(
            Some("rework"),
            Some("pending"),
            None,
            Some(&json!({
                "pm_decision": "rework",
                "comment": "Handle the edge case"
            })),
        );

        assert_eq!(
            summary.as_deref(),
            Some("PM requested rework: Handle the edge case")
        );
    }

    #[test]
    fn summarize_dispatch_result_handles_orphan_recovery() {
        let summary = summarize_dispatch_result(
            Some("implementation"),
            Some("completed"),
            Some(&json!({
                "auto_completed": true,
                "completion_source": "orphan_recovery"
            })),
            None,
        );

        assert_eq!(summary.as_deref(), Some("Recovered orphan dispatch"));
    }

    #[test]
    fn summarize_dispatch_result_handles_noop_completion() {
        let summary = summarize_dispatch_result(
            Some("implementation"),
            Some("completed"),
            Some(&json!({
                "work_outcome": "noop",
                "completed_without_changes": true,
                "notes": "spec already satisfied"
            })),
            None,
        );

        assert_eq!(summary.as_deref(), Some("No-op: spec already satisfied"));
    }
}
