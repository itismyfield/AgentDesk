use super::super::*;
use crate::services::memory::{RecallMode, RecallResponse};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct MemoryInjectionPlan<'a> {
    pub(super) shared_knowledge_for_context: Option<String>,
    pub(super) shared_knowledge_for_system_prompt: Option<String>,
    pub(super) external_recall_for_context: Option<&'a str>,
    pub(super) longterm_catalog_for_system_prompt: Option<&'a str>,
}

impl MemoryInjectionPlan<'_> {
    pub(super) fn sak_for_system_prompt(&self) -> Option<&str> {
        self.shared_knowledge_for_system_prompt.as_deref()
    }
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
    let shared_knowledge = crate::services::discord::shared_memory::load_shared_knowledge();
    let should_inject_shared_knowledge =
        dispatch_profile == DispatchProfile::Full && !has_session_id;
    let shared_knowledge_for_context =
        if should_inject_shared_knowledge && !matches!(provider, ProviderKind::Claude) {
            shared_knowledge.as_deref().map(str::to_owned)
        } else {
            None
        };
    let shared_knowledge_for_system_prompt =
        if dispatch_profile == DispatchProfile::Full && matches!(provider, ProviderKind::Claude) {
            shared_knowledge.as_deref().map(str::to_owned)
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

/// #4307 PR-B: wraps the voluntary tool_feedback reminder stashed at the end of
/// the previous turn into a labeled context block for the next turn's prompt.
/// Returns `None` for an empty reminder so the prompt is byte-for-byte unchanged
/// when nothing was stashed.
pub(super) fn format_voluntary_feedback_reminder(reminder: &str) -> Option<String> {
    let reminder = reminder.trim();
    if reminder.is_empty() {
        None
    } else {
        Some(format!(
            "[메모리 리마인더 — 이전 턴 recall 후 tool_feedback 미평가]\n\n{reminder}"
        ))
    }
}

/// #4196: wraps the turn-end WIP (uncommitted-changes) warning stashed at the
/// end of the previous turn into a labeled context block for the next turn's
/// prompt, so the agent is reminded to commit/stash before the worktree state is
/// lost. Returns `None` for an empty warning so the prompt is byte-for-byte
/// unchanged when nothing was stashed (clean worktree at the previous turn end).
pub(super) fn format_turn_end_wip_warning_injection(warning: &str) -> Option<String> {
    let warning = warning.trim();
    if warning.is_empty() {
        None
    } else {
        Some(format!(
            "[WIP 리마인더 — 이전 턴 종료 시 커밋되지 않은 변경사항 감지]\n\n{warning}"
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

#[allow(clippy::too_many_arguments)]
pub(super) fn build_race_requeued_intervention(
    request_owner: UserId,
    user_msg_id: MessageId,
    user_text: &str,
    preserve_on_cancel: bool,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
    pending_uploads: Vec<String>,
    // #2266: when the race-lost message is a voice-transcript announcement,
    // the per-process `voice::announce_meta` store entry was already consumed
    // by the active `handle_text_message` call before the race-loss branch
    // ran. Carry the announcement payload through the queued `Intervention`
    // so the dispatch path can reinsert it into the store before re-entering
    // `handle_text_message`, preserving voice-transcript framing instead of
    // degrading to plain text.
    voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
) -> Intervention {
    let queued_generation = crate::services::discord::runtime_store::process_generation();
    let source_generation = if preserve_on_cancel {
        crate::services::turn_orchestrator::SourceMessageQueuedGeneration::user_instruction(
            user_msg_id,
            queued_generation,
        )
    } else {
        crate::services::turn_orchestrator::SourceMessageQueuedGeneration::new(
            user_msg_id,
            queued_generation,
        )
    };
    Intervention {
        author_id: request_owner,
        author_is_bot: false,
        message_id: user_msg_id,
        queued_generation,
        source_message_ids: vec![user_msg_id],
        source_message_queued_generations: vec![source_generation],
        source_text_segments: Vec::new(),
        text: user_text.to_string(),
        mode: super::super::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context,
        has_reply_boundary,
        merge_consecutive,
        pending_uploads,
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

    #[test]
    fn race_requeue_carries_the_intake_cancel_preservation_decision() {
        let runtime_root = tempfile::tempdir().expect("race-requeue runtime root");
        let _root_guard = crate::config::set_agentdesk_root_for_test(runtime_root.path());
        let message_id = MessageId::new(4_247_201);

        let human = build_race_requeued_intervention(
            UserId::new(77),
            message_id,
            "keep this instruction",
            true,
            None,
            false,
            false,
            Vec::new(),
            None,
        );
        assert!(human.preserve_on_cancel());
        assert_eq!(human.source_message_queued_generations.len(), 1);
        assert!(human.source_message_queued_generations[0].preserve_on_cancel);

        let automation = build_race_requeued_intervention(
            UserId::new(78),
            MessageId::new(4_247_202),
            "DISPATCH:automation",
            false,
            None,
            false,
            false,
            Vec::new(),
            None,
        );
        assert!(!automation.preserve_on_cancel());
        assert!(!automation.source_message_queued_generations[0].preserve_on_cancel);
    }

    /// #4307 PR-B (a): a stashed reminder, run through the SAME assembly helpers
    /// the intake path uses (`format_voluntary_feedback_reminder` +
    /// `merge_reply_contexts`), must land in the reply context that feeds the
    /// next-turn prompt, with its labeled header and the raw reminder body.
    #[test]
    fn voluntary_feedback_reminder_is_injected_into_reply_context() {
        let reminder = "이번 턴 recall 2건 중 tool_feedback 0/2. 평가 후 턴 종료: [se-1, se-2]";
        let reply_context = merge_reply_contexts(
            Some("[Reply context] earlier discord quote".to_string()),
            format_voluntary_feedback_reminder(reminder),
        )
        .expect("reply context present when a reminder is stashed");

        assert!(
            reply_context.contains("[메모리 리마인더"),
            "the injected block must carry the reminder label"
        );
        assert!(
            reply_context.contains(reminder),
            "the injected block must carry the raw reminder body"
        );
        assert!(
            reply_context.contains("earlier discord quote"),
            "the pre-existing reply context must be preserved"
        );
        // merge prepends the reminder (secondary) ahead of the prior context.
        assert!(
            reply_context.find("[메모리 리마인더").unwrap()
                < reply_context.find("earlier discord quote").unwrap(),
            "the reminder must lead the merged reply context"
        );
    }

    /// #4307 PR-B (d): with no reminder stashed the reply context is byte-for-byte
    /// unchanged — `format_voluntary_feedback_reminder` yields `None` for empty
    /// input and `merge_reply_contexts` returns the primary untouched.
    #[test]
    fn absent_voluntary_feedback_reminder_leaves_reply_context_unchanged() {
        let base = "[Reply context] earlier discord quote".to_string();

        assert_eq!(format_voluntary_feedback_reminder("   "), None);
        assert_eq!(
            merge_reply_contexts(
                Some(base.clone()),
                format_voluntary_feedback_reminder("   "),
            ),
            Some(base.clone()),
        );
        // Nothing to inject and no prior context → the prompt gains no chunk.
        assert_eq!(
            merge_reply_contexts(None, format_voluntary_feedback_reminder("")),
            None,
        );
    }

    /// #4196: a WIP warning stashed at the previous turn's end, run through the
    /// SAME assembly helpers the intake path uses (`take_and_merge_wip_warning` →
    /// `format_turn_end_wip_warning_injection` + `merge_reply_contexts`), must
    /// land in the reply context that feeds the next-turn prompt so the agent is
    /// reminded to commit/stash its uncommitted changes. Mutation guard: if the
    /// injection is removed (format helper returns `None`), the `.expect` below
    /// panics and this test FAILS.
    #[test]
    fn wip_warning_is_injected_into_reply_context_when_uncommitted_changes_exist() {
        let warning = "⚠️ **턴을 완료하기 전에 커밋되지 않은 변경사항을 확인하세요.**\n\
             작업공간: `/tmp/wt`\n\
             파일 수: 스테이징됨 1개 · 스테이징 안 됨 1개 · 추적되지 않음 1개\n\
             턴을 끝내기 전에 변경사항을 커밋하거나 명시적으로 폐기하세요.";
        let reply_context = merge_reply_contexts(
            Some("[Reply context] earlier discord quote".to_string()),
            format_turn_end_wip_warning_injection(warning),
        )
        .expect("reply context present when a WIP warning is stashed");

        assert!(
            reply_context.contains("[WIP 리마인더"),
            "the injected block must carry the WIP reminder label"
        );
        assert!(
            reply_context.contains(warning),
            "the injected block must carry the raw WIP warning body"
        );
        assert!(
            reply_context.contains("earlier discord quote"),
            "the pre-existing reply context must be preserved"
        );
        // merge prepends the WIP warning (secondary) ahead of the prior context.
        assert!(
            reply_context.find("[WIP 리마인더").unwrap()
                < reply_context.find("earlier discord quote").unwrap(),
            "the WIP warning must lead the merged reply context"
        );
    }

    /// #4196: with a CLEAN worktree the previous turn stashes nothing, so the
    /// next turn's reply context is byte-for-byte unchanged —
    /// `format_turn_end_wip_warning_injection` yields `None` for empty input and
    /// `merge_reply_contexts` returns the primary untouched.
    #[test]
    fn absent_wip_warning_leaves_reply_context_unchanged_when_worktree_clean() {
        let base = "[Reply context] earlier discord quote".to_string();

        assert_eq!(format_turn_end_wip_warning_injection("   "), None);
        assert_eq!(
            merge_reply_contexts(
                Some(base.clone()),
                format_turn_end_wip_warning_injection("   "),
            ),
            Some(base.clone()),
        );
        // Nothing to inject and no prior context → the prompt gains no chunk.
        assert_eq!(
            merge_reply_contexts(None, format_turn_end_wip_warning_injection("")),
            None,
        );
    }

    #[test]
    fn issue_4310_sak_layer_is_independent_of_memento_recall_state() {
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _root_guard = crate::config::set_agentdesk_root_for_test(runtime_root.path());
        let sak_path = crate::runtime_layout::shared_agent_knowledge_path(runtime_root.path());
        std::fs::create_dir_all(sak_path.parent().expect("SAK parent")).expect("create SAK parent");
        std::fs::write(&sak_path, "state-independent rules").expect("write SAK");
        crate::services::discord::shared_memory::invalidate_shared_knowledge_cache_for_tests();
        let expected = "[Shared Agent Knowledge]\nstate-independent rules";
        let healthy_recall = RecallResponse {
            external_recall: Some("memento context".to_string()),
            memento_context_loaded: true,
            ..RecallResponse::default()
        };
        let degraded_recall = RecallResponse {
            warnings: vec!["memento unavailable; local fallback used".to_string()],
            ..RecallResponse::default()
        };

        for recall in [&healthy_recall, &degraded_recall] {
            let plan = build_memory_injection_plan(
                &ProviderKind::Claude,
                false,
                DispatchProfile::Full,
                recall,
            );
            assert_eq!(
                plan.shared_knowledge_for_system_prompt.as_deref(),
                Some(expected)
            );
        }
    }
}
