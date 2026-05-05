use chrono::{DateTime, FixedOffset, Utc};
use serde_json::Value;

use super::model::{InspectContextConfig, LatestTurn, LifecycleEventRow};
use crate::db::prompt_manifests::{PromptContentVisibility, PromptManifest};
use crate::services::discord::formatting::escape_for_code_fence;

const NO_RECENT_TURN_DATA: &str = "최근 턴 데이터 없음";
pub(super) const REPORT_LINE_MAX: usize = 100;
const ID_MAX_CHARS: usize = 28;

pub(super) fn format_context_usage(turn: &LatestTurn, context: &InspectContextConfig) -> String {
    let used = turn.context_occupancy_input_tokens();
    if context.context_window_tokens == 0 {
        return format!("unknown ({} tokens)", format_tokens(used as i64));
    }
    let pct = crate::services::discord::adk_session::context_usage_percent(
        used,
        context.context_window_tokens,
    );
    format!(
        "{}% ({} / {} tokens), compact threshold {}%",
        pct,
        format_tokens(used as i64),
        format_tokens(context.context_window_tokens as i64),
        context.compact_percent
    )
}

pub(super) fn format_prompt_summary(manifest: Option<&PromptManifest>) -> String {
    let Some(manifest) = manifest else {
        return "(없음)".to_string();
    };
    let profile = manifest.profile.as_deref().unwrap_or("(profile 없음)");
    format!(
        "{profile}, {} layers, {} tokens",
        manifest.layers.len(),
        format_tokens(manifest.total_input_tokens_est)
    )
}

pub(super) fn format_compaction(event: Option<&LifecycleEventRow>) -> String {
    let Some(event) = event else {
        return "(없음)".to_string();
    };
    let before = json_u64(&event.details_json, &["before_pct", "beforePct"]);
    let after = json_u64(&event.details_json, &["after_pct", "afterPct"]);
    match (before, after) {
        (Some(before), Some(after)) => {
            format!(
                "{} (before {}% -> after {}%)",
                format_kst(event.created_at),
                before,
                after
            )
        }
        _ => format!("{} ({})", format_kst(event.created_at), event.summary),
    }
}

pub(super) fn session_status_label(event: &LifecycleEventRow) -> &'static str {
    match event.kind.as_str() {
        "session_fresh" => "fresh",
        "session_resumed" => "resumed",
        "session_resume_failed_with_recovery" => "recovery",
        _ => "unknown",
    }
}

pub(super) fn session_id_from_event(event: Option<&LifecycleEventRow>) -> Option<&str> {
    event.and_then(|event| {
        json_str(
            &event.details_json,
            &[
                "provider_session_id",
                "providerSessionId",
                "raw_provider_session_id",
                "rawProviderSessionId",
                "session_id",
                "sessionId",
                "claude_session_id",
                "claudeSessionId",
            ],
        )
    })
}

pub(super) fn adk_session_from_event(event: Option<&LifecycleEventRow>) -> Option<&str> {
    event.and_then(|event| {
        json_str(
            &event.details_json,
            &[
                "recovered_session_key",
                "recoveredSessionKey",
                "previous_session_key",
                "previousSessionKey",
                "session_key",
                "sessionKey",
            ],
        )
    })
}

pub(super) fn tmux_action_label(event: Option<&LifecycleEventRow>) -> String {
    let Some(event) = event else {
        return "(없음)".to_string();
    };
    if let Some(value) = json_str(
        &event.details_json,
        &["tmux_action", "tmuxAction", "tmux", "backend_action"],
    ) {
        return value.to_string();
    }
    match event.kind.as_str() {
        "session_fresh" => "new session".to_string(),
        "session_resumed" => "reused existing".to_string(),
        "session_resume_failed_with_recovery" => "recovered after resume failure".to_string(),
        _ => "(없음)".to_string(),
    }
}

pub(super) fn human_recovery_source(source: &str) -> String {
    match source {
        "discord_recent" => "Discord recent messages".to_string(),
        other => other.to_string(),
    }
}

fn json_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn json_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}

pub(super) fn visibility_label(visibility: PromptContentVisibility) -> &'static str {
    match visibility {
        PromptContentVisibility::AdkProvided => "adk",
        PromptContentVisibility::UserDerived => "redacted",
    }
}

pub(super) fn fenced_report(body: impl Into<String>) -> String {
    let body = body.into();
    format!("```text\n{}\n```", escape_for_code_fence(body.trim_end()))
}

pub(super) fn no_data_report() -> String {
    fenced_report(NO_RECENT_TURN_DATA)
}

pub(super) fn push_kv(out: &mut String, key: &str, value: impl AsRef<str>) {
    push_line(out, &format!("{key}: {}", value.as_ref()));
}

pub(super) fn push_line(out: &mut String, line: &str) {
    out.push_str(&truncate_chars(line, REPORT_LINE_MAX));
    out.push('\n');
}

pub(super) fn opt_or_none(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| truncate_chars(value, ID_MAX_CHARS))
        .unwrap_or_else(|| "(없음)".to_string())
}

pub(super) fn format_duration(duration_ms: Option<i64>) -> String {
    let Some(duration_ms) = duration_ms.filter(|value| *value >= 0) else {
        return "(없음)".to_string();
    };
    if duration_ms < 1_000 {
        return format!("{duration_ms}ms");
    }
    let secs = duration_ms as f64 / 1_000.0;
    format!("{secs:.1}s")
}

pub(super) fn format_tokens(tokens: i64) -> String {
    if tokens.abs() >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens.abs() >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

pub(super) fn format_kst(value: DateTime<Utc>) -> String {
    let offset = FixedOffset::east_opt(9 * 60 * 60).expect("KST offset is valid");
    let kst = value.with_timezone(&offset);
    kst.format("%Y-%m-%d %H:%M KST").to_string()
}

pub(super) fn non_negative_u64(value: i64) -> u64 {
    u64::try_from(value.max(0)).unwrap_or(u64::MAX)
}

pub(super) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    format!("{}...", value.chars().take(keep).collect::<String>())
}

pub(super) fn wrap_line(line: &str, max_chars: usize) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    for word in line.split_whitespace() {
        let current_len = current.chars().count();
        let word_len = word.chars().count();
        if current_len > 0 && current_len + 1 + word_len > max_chars {
            result.push(current);
            current = String::new();
        }
        if word_len > max_chars {
            if !current.is_empty() {
                result.push(current);
                current = String::new();
            }
            result.push(truncate_chars(word, max_chars));
            continue;
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        result.push(current);
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}
