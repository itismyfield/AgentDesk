use super::super::*;

pub(super) fn format_session_retry_context(raw_context: &str) -> Option<String> {
    let raw_context = raw_context.trim();
    if raw_context.is_empty() {
        None
    } else {
        Some(format!(
            "[이전 대화 복원 — 새 세션 시작으로 최근 대화를 컨텍스트에 포함합니다]\n\n{raw_context}"
        ))
    }
}

pub(super) fn merge_reply_contexts(
    primary: Option<String>,
    secondary: Option<String>,
) -> Option<String> {
    match (primary, secondary) {
        (Some(primary), Some(secondary)) => Some(format!("{secondary}\n\n{primary}")),
        (Some(primary), None) => Some(primary),
        (None, Some(secondary)) => Some(secondary),
        (None, None) => None,
    }
}

pub(super) fn build_headless_trigger_context(
    source: Option<&str>,
    metadata: Option<&serde_json::Value>,
) -> Option<String> {
    let source = source.map(str::trim).filter(|value| !value.is_empty());
    let metadata = metadata.filter(|value| !value.is_null());
    if source.is_none() && metadata.is_none() {
        return None;
    }

    let mut lines = vec!["[Headless trigger context]".to_string()];
    if let Some(source) = source {
        lines.push(format!("source: {source}"));
    }
    if let Some(metadata) = metadata {
        lines.push(format!("metadata: {}", metadata));
    }
    Some(lines.join("\n"))
}

pub(super) fn build_system_discord_context(
    channel_name: Option<&str>,
    category_name: Option<&str>,
    channel_id: ChannelId,
    headless_fallback: bool,
) -> String {
    match channel_name {
        Some(name) => {
            let cat_part = category_name
                .map(|value| format!(" (category: {value})"))
                .unwrap_or_default();
            format!(
                "Discord context: channel #{} (ID: {}){}",
                name,
                channel_id.get(),
                cat_part
            )
        }
        None if headless_fallback => format!(
            "Discord context: headless channel {} (no bound channel name)",
            channel_id.get()
        ),
        None => "Discord context: DM".to_string(),
    }
}

fn normalize_turn_author_name(request_owner_name: &str, request_owner: UserId) -> String {
    let collapsed = request_owner_name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let base = if collapsed.is_empty() {
        format!("user {}", request_owner.get())
    } else {
        collapsed
    };
    let sanitized = base
        .chars()
        .map(|ch| match ch {
            '\r' | '\n' => ' ',
            '[' | '{' => '(',
            ']' | '}' => ')',
            _ => ch,
        })
        .collect::<String>();
    sanitized.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn wrap_user_prompt_with_author(
    request_owner_name: &str,
    request_owner: UserId,
    sanitized_prompt: String,
) -> String {
    let author = normalize_turn_author_name(request_owner_name, request_owner);
    format!(
        "[User: {author} (ID: {})]\n{}",
        request_owner.get(),
        sanitized_prompt
    )
}

pub(super) fn build_race_requeued_intervention(
    request_owner: UserId,
    user_msg_id: MessageId,
    user_text: &str,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
) -> Intervention {
    Intervention {
        author_id: request_owner,
        message_id: user_msg_id,
        source_message_ids: vec![user_msg_id],
        text: user_text.to_string(),
        mode: super::super::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context,
        has_reply_boundary,
        merge_consecutive,
    }
}
