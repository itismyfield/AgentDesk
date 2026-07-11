//! Shared renderer/sanitizer for Codex `<subagent_notification>` envelopes.

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::super::tui_task_card::{
    strip_terminal_controls, truncate_chars_ascii as truncate_chars,
};

const PREVIEW_CHARS: usize = 900;
const PREVIEW_LINES: usize = 8;

const RESUMED_THREAD_PROLOGUE: &str = "The prior authoritative Discord, role, and tool \
     instructions already present in this Codex thread still apply. Treat only this turn's \
     user request, reply context, uploaded files, and memory recall below as new actionable \
     input.";
const FRESH_FORK_PROLOGUE: &str = "The prior authoritative Discord, role, and tool \
     instructions already issued to this role in the current dcserver lifetime still apply. \
     Treat only this turn's user request, reply context, uploaded files, and memory recall \
     below as new actionable input.";

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default, alias = "task-id", alias = "taskId")]
    task_id: Option<String>,
    #[serde(default, alias = "tool-use-id", alias = "toolUseId")]
    tool_use_id: Option<String>,
    status: Option<Status>,
}

#[derive(Debug, Deserialize)]
struct Status {
    completed: Option<String>,
    failed: Option<String>,
}

enum Render {
    Completed(String),
    Failed(String),
    Unknown,
}

pub(in crate::services::discord) struct SubagentNotificationSemantic {
    pub(in crate::services::discord) task_id: Option<String>,
    pub(in crate::services::discord) tool_use_id: Option<String>,
    pub(in crate::services::discord) payload_fingerprint: String,
}

pub(in crate::services::discord) fn semantic_event(
    prompt: &str,
) -> Option<SubagentNotificationSemantic> {
    let payload = extract_payload(prompt).ok()?;
    let envelope: Envelope = serde_json::from_str(&payload).ok()?;
    let status = envelope.status?;
    let (status_name, report) = if let Some(report) = status
        .completed
        .as_deref()
        .map(str::trim)
        .filter(|report| !report.is_empty())
    {
        ("completed", report)
    } else if let Some(report) = status
        .failed
        .as_deref()
        .map(str::trim)
        .filter(|report| !report.is_empty())
    {
        ("failed", report)
    } else {
        ("unknown", "")
    };
    let task_id = clean_semantic_id(envelope.task_id);
    let tool_use_id = clean_semantic_id(envelope.tool_use_id);
    Some(SubagentNotificationSemantic {
        payload_fingerprint: fingerprint(&[
            task_id.as_deref().unwrap_or(""),
            tool_use_id.as_deref().unwrap_or(""),
            status_name,
            &strip_terminal_controls(report),
        ]),
        task_id,
        tool_use_id,
    })
}

fn clean_semantic_id(value: Option<String>) -> Option<String> {
    value
        .map(|value| strip_terminal_controls(&value).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn fingerprint(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
        .chars()
        .take(16)
        .collect()
}

pub(in crate::services::discord) fn is_start_anchored_subagent_notification(prompt: &str) -> bool {
    let normalized = normalized_start_anchored_injection(prompt);
    starts_with_xmlish_tag(&normalized, "subagent_notification")
}

pub(in crate::services::discord) fn format_subagent_notification_card(
    tmux_session_name: Option<&str>,
    prompt: &str,
) -> String {
    match parse(prompt) {
        Ok(Render::Completed(report)) => {
            format_report(tmux_session_name, "✅", "completed", &report)
        }
        Ok(Render::Failed(report)) => format_report(tmux_session_name, "❌", "failed", &report),
        Ok(Render::Unknown) => format!(
            "📋 Subagent notification{} — no completed/failed status",
            tmux_suffix(tmux_session_name),
        ),
        Err(_) => format!(
            "⚠️ Subagent notification{} — malformed payload omitted",
            tmux_suffix(tmux_session_name),
        ),
    }
}

pub(in crate::services::discord) fn sanitize_start_anchored_subagent_notification(
    input: &str,
) -> Option<String> {
    is_start_anchored_subagent_notification(input)
        .then(|| format_subagent_notification_card(None, input))
}

pub(in crate::services::discord) fn status_summary(input: &str) -> Option<String> {
    sanitize_start_anchored_subagent_notification(input).map(|card| {
        card.lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .unwrap_or("Subagent notification")
            .to_string()
    })
}

pub(in crate::services::discord) fn status_summary_from(
    current_tool_line: Option<&str>,
    full_response: &str,
) -> Option<String> {
    current_tool_line
        .and_then(status_summary)
        .or_else(|| status_summary(full_response))
}

pub(in crate::services::discord) fn streaming_rollover_should_skip(input: &str) -> bool {
    status_summary(input).is_some()
}

fn parse(prompt: &str) -> Result<Render, ()> {
    let payload = extract_payload(prompt)?;
    let envelope: Envelope = serde_json::from_str(&payload).map_err(|_| ())?;
    let status = envelope.status.ok_or(())?;
    if let Some(report) = status.completed.filter(|report| !report.trim().is_empty()) {
        return Ok(Render::Completed(report));
    }
    if let Some(report) = status.failed.filter(|report| !report.trim().is_empty()) {
        return Ok(Render::Failed(report));
    }
    Ok(Render::Unknown)
}

fn extract_payload(prompt: &str) -> Result<String, ()> {
    const OPEN: &str = "<subagent_notification";
    const CLOSE: &str = "</subagent_notification>";

    let normalized = normalized_start_anchored_injection(prompt);
    if !normalized.starts_with(OPEN) {
        return Err(());
    }
    let after_open_name = &normalized[OPEN.len()..];
    let Some(first) = after_open_name.chars().next() else {
        return Err(());
    };
    if first != '>' && !first.is_whitespace() {
        return Err(());
    }
    let open_end = normalized.find('>').ok_or(())? + 1;
    let after_open = &normalized[open_end..];
    let close_start = after_open.find(CLOSE).ok_or(())?;
    Ok(after_open[..close_start].trim().to_string())
}

fn normalized_start_anchored_injection(prompt: &str) -> String {
    let normalized = strip_terminal_controls(prompt);
    let mut current = normalized.trim_start().to_string();

    loop {
        let before = current.clone();

        let unwrapped = strip_leading_injection_wrapper(&current);
        if unwrapped != current {
            current = unwrapped.trim_start().to_string();
            continue;
        }

        if let Some(tail) = strip_provider_session_reuse_prologue(&current) {
            current = tail.trim_start().to_string();
            continue;
        }

        let stripped_chrome = super::strip_leading_tui_response_chrome(&current);
        if stripped_chrome != current {
            current = stripped_chrome.trim_start().to_string();
            continue;
        }

        if let Some(tail) = strip_leading_user_author_prefix(&current) {
            current = tail.trim_start().to_string();
            continue;
        }

        if current == before {
            break;
        }
    }

    current.trim_start().to_string()
}

fn strip_leading_injection_wrapper(text: &str) -> &str {
    const WRAPPER_MARKER: &str = "터미널에 직접 주입된 입력";
    if !text.starts_with(WRAPPER_MARKER) {
        return text;
    }
    let Some(after_wrapper_line) = text.find('\n').map(|idx| &text[idx + 1..]) else {
        return text;
    };
    let trimmed = after_wrapper_line.trim_start_matches(['\r', '\n']);
    if let Some(rest) = trimmed.strip_prefix("```") {
        if let Some(idx) = rest.find('\n') {
            return &rest[idx + 1..];
        }
        return after_wrapper_line;
    }
    after_wrapper_line
}

fn strip_provider_session_reuse_prologue(normalized: &str) -> Option<&str> {
    let rest = normalized
        .strip_prefix("[Provider Session Reuse]")?
        .trim_start();
    provider_reuse_tail(rest, RESUMED_THREAD_PROLOGUE)
        .or_else(|| provider_reuse_tail(rest, FRESH_FORK_PROLOGUE))
}

fn provider_reuse_tail<'a>(rest: &'a str, prologue: &str) -> Option<&'a str> {
    rest.strip_prefix(prologue)
        .and_then(|tail| tail.strip_prefix("\n\n"))
}

fn strip_leading_user_author_prefix(text: &str) -> Option<String> {
    let rest = text.strip_prefix("[User: ")?;
    let close = rest.find(']')?;
    let tail = rest[close + 1..].trim_start();
    let tail = super::strip_leading_tui_response_chrome(tail);
    starts_with_xmlish_tag(&tail, "subagent_notification").then_some(tail)
}

fn starts_with_xmlish_tag(text: &str, tag: &str) -> bool {
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(tag) else {
        return false;
    };
    rest.starts_with('>') || rest.chars().next().is_some_and(char::is_whitespace)
}

fn format_report(
    tmux_session_name: Option<&str>,
    icon: &str,
    status: &str,
    report: &str,
) -> String {
    let preview = preview(report);
    let mut lines = vec![format!(
        "{icon} Subagent {status}{}",
        tmux_suffix(tmux_session_name),
    )];
    if !preview.is_empty() {
        lines.push(String::new());
        lines.push(preview);
    }
    lines.join("\n")
}

fn preview(report: &str) -> String {
    let report = strip_terminal_controls(report);
    let mut collected: Vec<String> = Vec::new();
    for line in report.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        collected.push(sanitize_report_line(line));
        if collected.len() >= PREVIEW_LINES {
            break;
        }
    }
    truncate_chars(&collected.join("\n"), PREVIEW_CHARS)
}

fn sanitize_report_line(value: &str) -> String {
    value
        .replace('\r', " ")
        .replace('\n', " ")
        .replace("```", "` ` `")
}

fn tmux_suffix(tmux_session_name: Option<&str>) -> String {
    tmux_session_name
        .map(|name| format!(" (tmux : `{}`)", sanitize_inline_code(name)))
        .unwrap_or_default()
}

fn sanitize_inline_code(value: &str) -> String {
    value.replace('`', "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESUMED_PREFIX: &str = "[Provider Session Reuse]\nThe prior authoritative Discord, role, and tool instructions already present in this Codex thread still apply. Treat only this turn's user request, reply context, uploaded files, and memory recall below as new actionable input.\n\n";

    #[test]
    fn detects_bare_wrapped_and_provider_reuse_subagent_notifications() {
        let bare =
            r#"<subagent_notification>{"status":{"completed":"done"}}</subagent_notification>"#;
        assert!(is_start_anchored_subagent_notification(bare));

        let wrapped = format!("터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n{bare}\n```");
        assert!(is_start_anchored_subagent_notification(&wrapped));

        let resumed = format!("{RESUMED_PREFIX}{bare}");
        assert!(is_start_anchored_subagent_notification(&resumed));
    }

    #[test]
    fn detects_provider_reuse_user_prefixed_subagent_notifications_3777() {
        let raw =
            r#"<subagent_notification>{"status":{"completed":"done"}}</subagent_notification>"#;
        let prefixed = format!("{RESUMED_PREFIX}[User: 0hbujang (ID: 343742347365974026)] {raw}");
        assert!(is_start_anchored_subagent_notification(&prefixed));
        assert!(
            sanitize_start_anchored_subagent_notification(&prefixed)
                .expect("card")
                .contains("Subagent completed")
        );

        let wrapped = format!("터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n{prefixed}\n```");
        assert!(is_start_anchored_subagent_notification(&wrapped));
    }

    #[test]
    fn detects_provider_reuse_tui_chrome_subagent_notifications_3818() {
        let raw =
            r#"<subagent_notification>{"status":{"completed":"done"}}</subagent_notification>"#;
        let prefixed = format!("{RESUMED_PREFIX}No response requested.\n{raw}");
        assert!(is_start_anchored_subagent_notification(&prefixed));
        assert!(streaming_rollover_should_skip(&prefixed));
        assert_eq!(
            status_summary(&prefixed).as_deref(),
            Some("✅ Subagent completed")
        );
        assert!(
            sanitize_start_anchored_subagent_notification(&prefixed)
                .expect("card")
                .contains("Subagent completed")
        );

        let user_prefixed = format!(
            "{RESUMED_PREFIX}[User: 0hbujang (ID: 343742347365974026)] No response requested.\n{raw}"
        );
        assert!(is_start_anchored_subagent_notification(&user_prefixed));

        let chrome_before_user = format!(
            "{RESUMED_PREFIX}No response requested.\n[User: 0hbujang (ID: 343742347365974026)] {raw}"
        );
        assert!(is_start_anchored_subagent_notification(&chrome_before_user));
        assert!(streaming_rollover_should_skip(&chrome_before_user));
        let card = sanitize_start_anchored_subagent_notification(&chrome_before_user)
            .expect("chrome-before-user subagent card");
        assert!(card.contains("Subagent completed"));
        assert!(!card.contains("No response requested."));
        assert!(!card.contains("[User:"));
        assert!(!card.contains("<subagent_notification>"));
    }

    #[test]
    fn human_mid_body_quote_is_not_sanitized() {
        let quoted = "please inspect this log line:\n<subagent_notification>{\"status\":{\"completed\":\"x\"}}</subagent_notification>";
        assert!(!is_start_anchored_subagent_notification(quoted));
        assert!(sanitize_start_anchored_subagent_notification(quoted).is_none());

        let prefixed_human = "[User: 0hbujang] please inspect this log line:\n<subagent_notification>{\"status\":{\"completed\":\"x\"}}</subagent_notification>";
        assert!(!is_start_anchored_subagent_notification(prefixed_human));
        assert!(sanitize_start_anchored_subagent_notification(prefixed_human).is_none());
    }

    #[test]
    fn card_hides_raw_envelope_and_agent_path() {
        let prompt = r#"<subagent_notification>
{"agent_path":"/tmp/private-agent","status":{"completed":"Read-only review complete.\n\n1. Make /api/docs route-derived."}}
</subagent_notification>"#;

        let output = format_subagent_notification_card(Some("AgentDesk-codex"), prompt);

        assert!(output.contains("Subagent completed"));
        assert!(output.contains("Read-only review complete."));
        assert!(output.contains("1. Make /api/docs route-derived."));
        assert!(output.contains("(tmux : `AgentDesk-codex`)"));
        assert!(!output.contains("<subagent_notification>"));
        assert!(!output.contains("agent_path"));
        assert!(!output.contains("/tmp/private-agent"));
        assert!(!output.contains("{\""));
    }
}
