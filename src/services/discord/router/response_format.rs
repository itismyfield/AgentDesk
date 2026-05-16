use super::super::*;
use crate::services::memory::{RecallMode, RecallResponse};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct MemoryInjectionPlan<'a> {
    pub(super) shared_knowledge_for_context: Option<&'a str>,
    pub(super) shared_knowledge_for_system_prompt: Option<&'a str>,
    pub(super) external_recall_for_context: Option<&'a str>,
    pub(super) longterm_catalog_for_system_prompt: Option<&'a str>,
}

/// #1083: Memento recall gate decision.
///
/// Trigger conditions for full memento context injection:
/// 1. The user prompt contains a "previous-context" keyword.
/// 2. The user prompt contains an "error/failure" keyword.
/// 3. The user prompt contains a "settings change" keyword.
/// 4. The user prompt is an explicit recall command.
///
/// Non-memento backends always recall in `Full` mode for backwards
/// compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MementoRecallGateDecision {
    pub(super) should_recall: bool,
    pub(super) mode: RecallMode,
    pub(super) reason: &'static str,
}

pub(super) fn build_memory_injection_plan<'a>(
    provider: &ProviderKind,
    has_session_id: bool,
    dispatch_profile: DispatchProfile,
    memory_recall: &'a RecallResponse,
) -> MemoryInjectionPlan<'a> {
    let should_inject_shared_knowledge =
        dispatch_profile == DispatchProfile::Full && !has_session_id;
    let shared_knowledge_for_context =
        if should_inject_shared_knowledge && !matches!(provider, ProviderKind::Claude) {
            memory_recall.shared_knowledge.as_deref()
        } else {
            None
        };
    let shared_knowledge_for_system_prompt =
        if dispatch_profile == DispatchProfile::Full && matches!(provider, ProviderKind::Claude) {
            memory_recall.shared_knowledge.as_deref()
        } else {
            None
        };
    let external_recall_for_context = if dispatch_profile != DispatchProfile::ReviewLite {
        memory_recall.external_recall.as_deref()
    } else {
        None
    };
    let longterm_catalog_for_system_prompt = if dispatch_profile == DispatchProfile::Full {
        memory_recall.longterm_catalog.as_deref()
    } else {
        None
    };

    MemoryInjectionPlan {
        shared_knowledge_for_context,
        shared_knowledge_for_system_prompt,
        external_recall_for_context,
        longterm_catalog_for_system_prompt,
    }
}

pub(super) fn memento_recall_gate_decision(
    memory_settings: &settings::ResolvedMemorySettings,
    memento_context_loaded: bool,
    user_text: &str,
    dispatch_profile: DispatchProfile,
) -> MementoRecallGateDecision {
    if memory_settings.backend != settings::MemoryBackendKind::Memento {
        return MementoRecallGateDecision {
            should_recall: true,
            mode: RecallMode::Full,
            reason: "non_memento_backend",
        };
    }

    if dispatch_profile == DispatchProfile::ReviewLite {
        return MementoRecallGateDecision {
            should_recall: false,
            mode: RecallMode::Full,
            reason: "review_lite_profile",
        };
    }

    let normalized = user_text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = normalized.to_lowercase();
    let text = lower.as_str();

    if ["이전에", "저번에", "전에"]
        .iter()
        .any(|keyword| text.contains(keyword))
    {
        return MementoRecallGateDecision {
            should_recall: true,
            mode: RecallMode::Full,
            reason: "previous_context_signal",
        };
    }

    if ["에러", "실패", "오류", "안 됨", "안됨"]
        .iter()
        .any(|keyword| text.contains(keyword))
    {
        return MementoRecallGateDecision {
            should_recall: true,
            mode: RecallMode::Full,
            reason: "error_context_signal",
        };
    }

    if [
        "설정 변경",
        "설정 바",
        "설정 업데이트",
        "config change",
        "configuration change",
        "settings change",
    ]
    .iter()
    .any(|keyword| text.contains(keyword))
    {
        return MementoRecallGateDecision {
            should_recall: true,
            mode: RecallMode::Full,
            reason: "setting_change_signal",
        };
    }

    let trimmed = text.trim_start();
    if trimmed.starts_with("/recall")
        || trimmed.starts_with("/memento")
        || trimmed.starts_with("/memory-read")
        || text.contains("[memento:recall]")
        || text.contains("<memento:recall>")
        || text.contains("memento_recall")
        || text.contains("@memento recall")
    {
        return MementoRecallGateDecision {
            should_recall: true,
            mode: RecallMode::Full,
            reason: "explicit_recall_signal",
        };
    }

    if !memento_context_loaded {
        return MementoRecallGateDecision {
            should_recall: true,
            mode: RecallMode::IdentityOnly,
            reason: if dispatch_profile == DispatchProfile::Lite {
                "lite_identity_only"
            } else {
                "identity_only_session_start"
            },
        };
    }

    MementoRecallGateDecision {
        should_recall: false,
        mode: RecallMode::Full,
        reason: if dispatch_profile == DispatchProfile::Lite {
            "lite_no_turn_signal"
        } else {
            "no_turn_signal"
        },
    }
}

pub(super) fn dispatch_profile_label(dispatch_profile: DispatchProfile) -> &'static str {
    match dispatch_profile {
        DispatchProfile::Full => "full",
        DispatchProfile::Lite => "lite",
        DispatchProfile::ReviewLite => "review_lite",
    }
}

pub(super) fn should_note_memento_context_loaded(
    memory_settings: &settings::ResolvedMemorySettings,
    memento_context_loaded: bool,
    memory_recall: &RecallResponse,
) -> bool {
    memory_settings.backend == settings::MemoryBackendKind::Memento
        && !memento_context_loaded
        && memory_recall.memento_context_loaded
}

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
    let prefix = format!("[User: {author} (ID: {})]", request_owner.get());
    let normalized_prompt = sanitized_prompt.replace("\r\n", "\n").replace('\r', "\n");
    if normalized_prompt.is_empty() {
        prefix
    } else if normalized_prompt.contains('\n') {
        format!("{prefix}\n{normalized_prompt}")
    } else {
        // Keep ordinary Discord messages on one terminal line so TUI input can
        // use literal send-keys instead of multiline paste-buffer submission.
        format!("{prefix} {normalized_prompt}")
    }
}

pub(super) fn build_race_requeued_intervention(
    request_owner: UserId,
    user_msg_id: MessageId,
    user_text: &str,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
    // #2266: when the race-lost message is a voice-transcript announcement,
    // the per-process `voice::announce_meta` store entry was already consumed
    // by the active `handle_text_message` call before the race-loss branch
    // ran. Carry the announcement payload through the queued `Intervention`
    // so the dispatch path can reinsert it into the store before re-entering
    // `handle_text_message`, preserving voice-transcript framing instead of
    // degrading to plain text.
    voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
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
        voice_announcement,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poise::serenity_prelude::UserId;

    #[test]
    fn wrap_user_prompt_with_author_keeps_single_line_body_inline() {
        let prompt = wrap_user_prompt_with_author(
            "  Alice [ops]\nteam  ",
            UserId::new(77),
            "deploy it".to_string(),
        );

        assert_eq!(prompt, "[User: Alice (ops) team (ID: 77)] deploy it");
    }

    #[test]
    fn wrap_user_prompt_with_author_preserves_multiline_body() {
        let prompt =
            wrap_user_prompt_with_author("Alice", UserId::new(77), "line 1\r\nline 2".to_string());

        assert_eq!(prompt, "[User: Alice (ID: 77)]\nline 1\nline 2");
    }
}
