//! Completion report cards retain quoted body previews and structured usage.

use super::{
    TaskNotification, clamp_discord_message_content, decode_entities_once, extract_pr_urls,
    format_duration_ms, has_completion_body, render_result_preview, sanitize_multiline,
    sanitize_oneline, status_icon,
};

pub(in crate::services::discord) fn format_task_notification_card(
    note: &TaskNotification,
    update_count: u64,
) -> String {
    render(note, update_count, has_completion_body(note))
}

pub(in crate::services::discord) fn format_legacy(
    note: &TaskNotification,
    update_count: u64,
) -> String {
    render(note, update_count, false)
}

fn render(note: &TaskNotification, update_count: u64, completion: bool) -> String {
    let status = note.status.as_deref().unwrap_or("");
    let icon = status_icon(status);
    let status_label = if status.is_empty() {
        "Task event".to_string()
    } else {
        format!("Task {status}")
    };
    let mut header = if completion {
        let summary = note
            .summary
            .as_deref()
            .filter(|summary| !summary.trim().is_empty());
        let title = summary.map_or(status_label.clone(), |summary| {
            sanitize_oneline(&decode_entities_once(summary))
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        });
        format!("{icon} **{title}**")
    } else {
        format!("{icon} {status_label}")
    };
    if update_count > 1 {
        header.push_str(&format!(" · updated {update_count}x"));
    }
    let mut lines = vec![header];
    if !completion && let Some(summary) = note.summary.as_deref().filter(|s| !s.is_empty()) {
        lines.push(format!(
            "**{}**",
            sanitize_multiline(&decode_entities_once(summary))
        ));
    }
    if let Some(result) = note.result.as_deref().filter(|s| !s.is_empty()) {
        // Each fresh render decodes one harness escape layer before producing the preview.
        let result = decode_entities_once(result);
        let body = render_result_preview(&result, update_count > 1);
        if !body.is_empty() {
            lines.push(String::new());
            lines.push(body);
        }
        for url in extract_pr_urls(&result) {
            lines.push(format!("🔗 {url}"));
        }
    }
    let mut footer_parts = Vec::new();
    if !completion && let Some(task_id) = note.task_id.as_deref().filter(|s| !s.is_empty()) {
        let short_id = task_id.trim().chars().take(8).collect::<String>();
        footer_parts.push(format!("task {short_id}"));
    }
    if let Some(agents) = note
        .agent_count
        .as_deref()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 1)
    {
        footer_parts.push(format!("{agents} agents"));
    }
    if let Some(tool_uses) = note.tool_uses.as_deref().filter(|s| !s.is_empty()) {
        footer_parts.push(format!("{} tools", sanitize_oneline(tool_uses)));
    }
    if let Some(duration) = note.duration_ms.as_deref().and_then(format_duration_ms) {
        footer_parts.push(format!("⏱ {duration}"));
    } else if let Some(duration) = note.duration.as_deref().filter(|s| !s.is_empty()) {
        footer_parts.push(format!("⏱ {}", sanitize_oneline(duration)));
    }
    if completion
        && let Some(tokens) = note
            .subagent_tokens
            .as_deref()
            .and_then(|raw| raw.trim().parse::<u128>().ok())
    {
        footer_parts.push(format!("🪙 {} tok", compact_tokens(tokens)));
    }
    // An aggregate token total cannot price a model's separate input and output usage.
    if note.subagent_tokens.is_none()
        && note.agent_count.is_none()
        && let Some(usage) = note
            .usage
            .as_deref()
            .filter(|s| !s.is_empty() && !s.contains('<'))
            .filter(|s| {
                let normalized = s.to_ascii_lowercase();
                !normalized.contains(" tok") && !normalized.contains("token")
            })
    {
        footer_parts.push(sanitize_oneline(usage));
    }
    if !footer_parts.is_empty() {
        lines.push(String::new());
        lines.push(format!("-# {}", footer_parts.join(" · ")));
    }
    clamp_discord_message_content(&lines.join("\n"))
}

fn compact_tokens(tokens: u128) -> String {
    for (scale, suffix) in [
        (1_000_000_000_000, "t"),
        (1_000_000_000, "b"),
        (1_000_000, "m"),
        (1_000, "k"),
    ] {
        if tokens >= scale {
            let whole = tokens / scale;
            let decimal = (tokens % scale) / (scale / 10);
            return if decimal == 0 {
                format!("{whole}{suffix}")
            } else {
                format!("{whole}.{decimal}{suffix}")
            };
        }
    }
    tokens.to_string()
}
