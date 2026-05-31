use super::super::super::DiscordSession;
use super::super::control_intent::{
    build_control_intent_system_reminder, detect_natural_language_control_intent,
};
use super::*;
use crate::services::discord::prompt_builder;
use crate::services::memory::RecallResponse;
use crate::ui::ai_screen::{HistoryItem, HistoryType};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::sync::Arc;
use std::time::Duration;

fn sample_recall() -> RecallResponse {
    RecallResponse {
        shared_knowledge: Some("[Shared Knowledge]".to_string()),
        longterm_catalog: Some("- notes.md".to_string()),
        external_recall: Some("[External Recall]".to_string()),
        memento_context_loaded: true,
        warnings: Vec::new(),
        token_usage: crate::services::memory::TokenUsage::default(),
    }
}

fn make_session(
    current_path: Option<String>,
    remote_profile_name: Option<String>,
) -> DiscordSession {
    DiscordSession {
        session_id: None,
        memento_context_loaded: false,
        memento_reflected: false,
        current_path,
        history: Vec::new(),
        pending_uploads: Vec::new(),
        cleared: false,
        remote_profile_name,
        channel_id: None,
        channel_name: None,
        category_name: None,
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: 0,
        assistant_turns: 0,
    }
}

#[test]
fn headless_turn_message_id_seed_uses_time_and_process() {
    let seed = headless_turn_message_id_seed(1_777_500_000_000, 42);
    let later_seed = headless_turn_message_id_seed(1_777_500_000_001, 42);
    let other_process_seed = headless_turn_message_id_seed(1_777_500_000_000, 43);

    assert!(seed >= HEADLESS_TURN_MESSAGE_ID_BASE);
    assert!(later_seed > seed);
    assert_ne!(seed, other_process_seed);
}

#[test]
fn claude_tui_busy_followup_notice_names_enqueue_refusal_reason() {
    use crate::services::turn_orchestrator::EnqueueRefusalReason;

    assert_eq!(
        claude_tui_busy_followup_refusal_notice(Some(EnqueueRefusalReason::SourceIdAlreadyQueued)),
        CLAUDE_TUI_BUSY_FOLLOWUP_ALREADY_QUEUED_NOTICE
    );
    assert_eq!(
        claude_tui_busy_followup_refusal_notice(Some(EnqueueRefusalReason::LastItemDedup)),
        CLAUDE_TUI_BUSY_FOLLOWUP_DEDUP_NOTICE
    );
    assert_eq!(
        claude_tui_busy_followup_refusal_notice(Some(EnqueueRefusalReason::ActorUnreachable)),
        CLAUDE_TUI_BUSY_FOLLOWUP_QUEUE_UNREACHABLE_NOTICE
    );
    assert_eq!(
        claude_tui_busy_followup_refusal_notice(None),
        CLAUDE_TUI_BUSY_FOLLOWUP_NOTICE
    );
}

#[test]
fn tui_hosting_runtime_kind_mismatch_requires_session_recreate() {
    assert!(runtime_kind_mismatch_requires_recreate(
        Some(RuntimeHandoffKind::ClaudeTui),
        Some(RuntimeHandoffKind::LegacyTmuxWrapper)
    ));
    assert!(runtime_kind_mismatch_requires_recreate(
        Some(RuntimeHandoffKind::LegacyTmuxWrapper),
        Some(RuntimeHandoffKind::CodexTui)
    ));
    assert!(!runtime_kind_mismatch_requires_recreate(
        Some(RuntimeHandoffKind::CodexTui),
        Some(RuntimeHandoffKind::CodexTui)
    ));
    assert!(!runtime_kind_mismatch_requires_recreate(
        None,
        Some(RuntimeHandoffKind::CodexTui)
    ));
    assert!(!runtime_kind_mismatch_requires_recreate(
        Some(RuntimeHandoffKind::CodexTui),
        None
    ));
}

#[test]
fn metadata_delivery_bot_uses_safe_explicit_bot_only() {
    let explicit = serde_json::json!({
        "delivery_bot": " opencode ",
        "agent_id": "fallback"
    });
    assert_eq!(
        metadata_delivery_bot(Some(&explicit)).as_deref(),
        Some("opencode")
    );

    let fallback = serde_json::json!({"agent_id": "monitoring"});
    assert_eq!(metadata_delivery_bot(Some(&fallback)), None);

    let invalid = serde_json::json!({"delivery_bot": "not valid"});
    assert_eq!(metadata_delivery_bot(Some(&invalid)), None);
}

#[test]
fn metadata_turn_source_prefers_explicit_source_arg() {
    let metadata = serde_json::json!({"source": "text"});

    assert_eq!(
        metadata_turn_source(Some("voice"), Some(&metadata)),
        crate::dispatch::Source::Voice
    );
    assert_eq!(
        metadata_turn_source(None, Some(&metadata)),
        crate::dispatch::Source::Text
    );
    assert_eq!(
        metadata_turn_source(None, None),
        crate::dispatch::Source::Text
    );
}

#[test]
fn memory_injection_plan_routes_shared_knowledge_by_provider() {
    let recall = sample_recall();

    let claude =
        build_memory_injection_plan(&ProviderKind::Claude, false, DispatchProfile::Full, &recall);
    assert_eq!(claude.shared_knowledge_for_context, None);
    assert_eq!(
        claude.shared_knowledge_for_system_prompt,
        Some("[Shared Knowledge]")
    );
    assert_eq!(
        claude.external_recall_for_context,
        Some("[External Recall]")
    );
    assert_eq!(
        claude.longterm_catalog_for_system_prompt,
        Some("- notes.md")
    );

    let codex =
        build_memory_injection_plan(&ProviderKind::Codex, false, DispatchProfile::Full, &recall);
    assert_eq!(
        codex.shared_knowledge_for_context,
        Some("[Shared Knowledge]")
    );
    assert_eq!(codex.shared_knowledge_for_system_prompt, None);
    assert_eq!(codex.external_recall_for_context, Some("[External Recall]"));
    assert_eq!(codex.longterm_catalog_for_system_prompt, Some("- notes.md"));

    let qwen =
        build_memory_injection_plan(&ProviderKind::Qwen, false, DispatchProfile::Full, &recall);
    assert_eq!(
        qwen.shared_knowledge_for_context,
        Some("[Shared Knowledge]")
    );
    assert_eq!(qwen.shared_knowledge_for_system_prompt, None);
    assert_eq!(qwen.external_recall_for_context, Some("[External Recall]"));
    assert_eq!(qwen.longterm_catalog_for_system_prompt, Some("- notes.md"));
}

#[test]
fn memory_injection_plan_keeps_review_lite_minimal() {
    let recall = sample_recall();
    let plan = build_memory_injection_plan(
        &ProviderKind::Codex,
        false,
        DispatchProfile::ReviewLite,
        &recall,
    );

    assert_eq!(plan.shared_knowledge_for_context, None);
    assert_eq!(plan.shared_knowledge_for_system_prompt, None);
    assert_eq!(plan.external_recall_for_context, None);
    assert_eq!(plan.longterm_catalog_for_system_prompt, None);
}

#[test]
fn memory_injection_plan_keeps_lite_to_external_recall_only() {
    let recall = sample_recall();
    let plan =
        build_memory_injection_plan(&ProviderKind::Codex, false, DispatchProfile::Lite, &recall);

    assert_eq!(plan.shared_knowledge_for_context, None);
    assert_eq!(plan.shared_knowledge_for_system_prompt, None);
    assert_eq!(plan.external_recall_for_context, Some("[External Recall]"));
    assert_eq!(plan.longterm_catalog_for_system_prompt, None);
}

#[test]
fn memory_injection_plan_skips_shared_knowledge_when_session_exists() {
    let recall = sample_recall();
    let plan =
        build_memory_injection_plan(&ProviderKind::Codex, true, DispatchProfile::Full, &recall);

    assert_eq!(plan.shared_knowledge_for_context, None);
    assert_eq!(plan.shared_knowledge_for_system_prompt, None);
    assert_eq!(plan.external_recall_for_context, Some("[External Recall]"));
    assert_eq!(plan.longterm_catalog_for_system_prompt, Some("- notes.md"));
}

#[test]
fn memory_injection_plan_keeps_shared_knowledge_for_claude_resumed_sessions() {
    let recall = sample_recall();
    let plan =
        build_memory_injection_plan(&ProviderKind::Claude, true, DispatchProfile::Full, &recall);

    assert_eq!(plan.shared_knowledge_for_context, None);
    assert_eq!(
        plan.shared_knowledge_for_system_prompt,
        Some("[Shared Knowledge]")
    );
    assert_eq!(plan.external_recall_for_context, Some("[External Recall]"));
    assert_eq!(plan.longterm_catalog_for_system_prompt, Some("- notes.md"));
}

#[test]
fn resolve_session_id_for_current_turn_drops_resume_after_model_reset() {
    assert_eq!(
        resolve_session_id_for_current_turn(Some("session-123".to_string()), true),
        None
    );
}

#[test]
fn resolve_session_id_for_current_turn_keeps_existing_session_when_not_reset() {
    assert_eq!(
        resolve_session_id_for_current_turn(Some("session-123".to_string()), false),
        Some("session-123".to_string())
    );
}

#[test]
fn memory_injection_plan_treats_model_reset_as_fresh_turn() {
    let recall = sample_recall();
    let session_id = resolve_session_id_for_current_turn(Some("session-123".to_string()), true);
    let plan = build_memory_injection_plan(
        &ProviderKind::Codex,
        session_id.is_some(),
        DispatchProfile::Full,
        &recall,
    );

    assert_eq!(
        plan.shared_knowledge_for_context,
        Some("[Shared Knowledge]")
    );
    assert_eq!(plan.external_recall_for_context, Some("[External Recall]"));
}

#[test]
fn session_path_is_usable_for_existing_local_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = make_session(Some(dir.path().to_str().unwrap().to_string()), None);
    assert!(session.validated_path("test-channel").is_some());
}

#[test]
fn session_path_is_not_usable_for_missing_local_path() {
    let dir = tempfile::tempdir().unwrap();
    let missing_path = dir.path().to_str().unwrap().to_string();
    drop(dir);
    let mut session = make_session(Some(missing_path), None);
    assert!(session.validated_path("test-channel").is_none());
    assert!(session.current_path.is_none());
    assert!(session.worktree.is_none());
}

#[test]
fn session_path_is_stale_for_remote_session_with_missing_local_path() {
    let dir = tempfile::tempdir().unwrap();
    let missing_path = dir.path().to_str().unwrap().to_string();
    drop(dir);
    let mut session = make_session(Some(missing_path), Some("mac-mini".to_string()));
    assert!(session.validated_path("test-channel").is_some());
    assert!(session.current_path.is_some());
}

#[test]
fn review_bypass_hint_detects_leading_pr_number_direct_merge_request() {
    let hint =
        detect_natural_language_control_intent("366은 기여자가 직접 머지가능하게 만들 것 같아")
            .map(|intent| build_control_intent_system_reminder(&intent))
            .expect("direct merge intent should be detected");

    assert!(hint.contains("pr_number: 366"));
    assert!(hint.contains("review_decision: dismiss"));
}

#[test]
fn review_bypass_hint_detects_explicit_pr_reference() {
    let hint = detect_natural_language_control_intent("#366 리뷰 우회하고 직접 머지해도 돼")
        .map(|intent| build_control_intent_system_reminder(&intent))
        .expect("explicit PR reference should be detected");

    assert!(hint.contains("PR #366"));
}

#[test]
fn review_bypass_hint_ignores_debug_discussion() {
    assert_eq!(
        detect_natural_language_control_intent("366 리뷰 우회 인식이 왜 안먹었는지 잡아줘"),
        None
    );
}

#[test]
fn review_bypass_hint_ignores_negative_direct_merge_request() {
    assert_eq!(
        detect_natural_language_control_intent("#366 리뷰 우회하면 안 돼"),
        None
    );
    assert_eq!(
        detect_natural_language_control_intent("366은 직접 머지하지 마"),
        None
    );
}

#[test]
fn review_bypass_hint_ignores_stray_non_pr_numbers() {
    assert_eq!(
        detect_natural_language_control_intent("2명만 직접 머지 가능하게 해줘"),
        None
    );
}

#[test]
fn memento_recall_gate_uses_session_start_and_turn_signals() {
    let memento = settings::ResolvedMemorySettings {
        backend: settings::MemoryBackendKind::Memento,
        ..settings::ResolvedMemorySettings::default()
    };
    let file = settings::ResolvedMemorySettings::default();

    // #1083: a fresh session (no memento context loaded yet) without any
    // turn signal should trigger the *identity-only* lite recall, not the
    // full session_start recall.
    let identity =
        memento_recall_gate_decision(&memento, false, "평범한 요청", DispatchProfile::Full);
    assert_eq!(identity.reason, "identity_only_session_start");
    assert!(identity.should_recall);
    assert_eq!(identity.mode, RecallMode::IdentityOnly);

    // After identity is loaded, no trigger means no recall.
    assert!(
        !memento_recall_gate_decision(&memento, true, "평범한 요청", DispatchProfile::Full)
            .should_recall
    );

    // Trigger keywords still upgrade to full recall regardless of whether
    // identity has been loaded yet.
    let prev = memento_recall_gate_decision(
        &memento,
        true,
        "이전에 하던 거 이어서 해줘",
        DispatchProfile::Full,
    );
    assert_eq!(prev.reason, "previous_context_signal");
    assert_eq!(prev.mode, RecallMode::Full);

    let err = memento_recall_gate_decision(
        &memento,
        true,
        "빌드 실패 원인 찾아줘",
        DispatchProfile::Full,
    );
    assert_eq!(err.reason, "error_context_signal");
    assert_eq!(err.mode, RecallMode::Full);

    let cfg = memento_recall_gate_decision(
        &memento,
        true,
        "설정 변경 내용 기억나?",
        DispatchProfile::Full,
    );
    assert_eq!(cfg.reason, "setting_change_signal");
    assert_eq!(cfg.mode, RecallMode::Full);

    let explicit =
        memento_recall_gate_decision(&memento, true, "/recall deploy note", DispatchProfile::Full);
    assert_eq!(explicit.reason, "explicit_recall_signal");
    assert_eq!(explicit.mode, RecallMode::Full);

    // Trigger keywords on a fresh session also win over identity-only.
    let fresh_trigger = memento_recall_gate_decision(
        &memento,
        false,
        "이전에 하던 거 이어서 해줘",
        DispatchProfile::Full,
    );
    assert_eq!(fresh_trigger.reason, "previous_context_signal");
    assert_eq!(fresh_trigger.mode, RecallMode::Full);

    // Non-memento backend always recalls in Full mode.
    let non_memento =
        memento_recall_gate_decision(&file, true, "평범한 요청", DispatchProfile::Full);
    assert!(non_memento.should_recall);
    assert_eq!(non_memento.mode, RecallMode::Full);
}

#[test]
fn memento_recall_gate_keeps_lite_profile_lightweight_without_trigger() {
    let memento = settings::ResolvedMemorySettings {
        backend: settings::MemoryBackendKind::Memento,
        ..settings::ResolvedMemorySettings::default()
    };

    let first = memento_recall_gate_decision(&memento, false, "평범한 요청", DispatchProfile::Lite);
    assert!(first.should_recall);
    assert_eq!(first.reason, "lite_identity_only");
    assert_eq!(first.mode, RecallMode::IdentityOnly);

    let next = memento_recall_gate_decision(&memento, true, "평범한 요청", DispatchProfile::Lite);
    assert!(!next.should_recall);
    assert_eq!(next.reason, "lite_no_turn_signal");
}

#[test]
fn memento_recall_gate_lite_profile_keeps_explicit_full_recall_triggers() {
    let memento = settings::ResolvedMemorySettings {
        backend: settings::MemoryBackendKind::Memento,
        ..settings::ResolvedMemorySettings::default()
    };

    let prev = memento_recall_gate_decision(
        &memento,
        true,
        "이전에 하던 거 이어서 해줘",
        DispatchProfile::Lite,
    );
    assert!(prev.should_recall);
    assert_eq!(prev.reason, "previous_context_signal");
    assert_eq!(prev.mode, RecallMode::Full);

    let explicit =
        memento_recall_gate_decision(&memento, true, "/recall deploy note", DispatchProfile::Lite);
    assert!(explicit.should_recall);
    assert_eq!(explicit.reason, "explicit_recall_signal");
    assert_eq!(explicit.mode, RecallMode::Full);
}

#[test]
fn memento_context_loaded_is_not_noted_without_explicit_backend_success() {
    let settings = settings::ResolvedMemorySettings {
        backend: settings::MemoryBackendKind::Memento,
        ..settings::ResolvedMemorySettings::default()
    };

    assert!(!should_note_memento_context_loaded(
        &settings,
        false,
        &RecallResponse::default()
    ));

    let recall = RecallResponse {
        memento_context_loaded: true,
        ..RecallResponse::default()
    };
    assert!(should_note_memento_context_loaded(
        &settings, false, &recall
    ));
    assert!(!should_note_memento_context_loaded(
        &settings, true, &recall
    ));
}

#[test]
fn dispatch_turns_add_pending_reaction_as_single_source() {
    // #750: announce bot no longer writes ⏳. Command bot must add it on
    // dispatch turn start so the stop-via-reaction-removal path still
    // works.
    let dispatch_id = crate::services::discord::adk_session::parse_dispatch_id(
        "DISPATCH:550e8400-e29b-41d4-a716-446655440000 - Fix login bug",
    );

    assert!(should_add_turn_pending_reaction(dispatch_id.as_deref()));
}

#[test]
fn regular_turns_keep_generic_pending_reaction() {
    assert!(should_add_turn_pending_reaction(None));
}

#[test]
fn native_fast_mode_override_only_applies_when_explicitly_enabled() {
    assert_eq!(
        native_fast_mode_override_for_turn(&ProviderKind::Claude, Some(true)),
        Some(true)
    );
    assert_eq!(
        native_fast_mode_override_for_turn(&ProviderKind::Claude, Some(false)),
        Some(false)
    );
    assert_eq!(
        native_fast_mode_override_for_turn(&ProviderKind::Claude, None),
        None
    );
    assert_eq!(
        native_fast_mode_override_for_turn(&ProviderKind::Gemini, Some(true)),
        None
    );
}

#[test]
fn codex_goals_override_only_applies_to_codex() {
    assert_eq!(
        codex_goals_override_for_turn(&ProviderKind::Codex, Some(true)),
        Some(true)
    );
    assert_eq!(
        codex_goals_override_for_turn(&ProviderKind::Codex, Some(false)),
        Some(false)
    );
    assert_eq!(
        codex_goals_override_for_turn(&ProviderKind::Claude, Some(true)),
        None
    );
}

#[test]
fn codex_goal_start_request_matches_only_goal_command_prefix() {
    assert!(is_codex_goal_start_request("/goal"));
    assert!(is_codex_goal_start_request("  /goal 지금 문서 검토"));
    assert!(is_codex_goal_start_request("/goal\n다음 줄"));
    assert!(is_codex_goal_start_request("/goal\t탭 뒤 내용"));

    assert!(!is_codex_goal_start_request("/goals"));
    assert!(!is_codex_goal_start_request("/goalkeeper"));
    assert!(!is_codex_goal_start_request("질문 /goal"));
    assert!(!is_codex_goal_start_request(""));
}

#[test]
fn classify_codex_goal_command_basic() {
    // ChainedStart: plain /goal
    assert_eq!(
        classify_codex_goal_command("/goal 새 목표"),
        GoalCommandKind::ChainedStart
    );
    assert_eq!(
        classify_codex_goal_command("/goal\n다음 줄"),
        GoalCommandKind::ChainedStart
    );
    assert_eq!(
        classify_codex_goal_command("  /goal 탭 뒤"),
        GoalCommandKind::ChainedStart
    );

    // FreshStart: /goal --fresh
    assert_eq!(
        classify_codex_goal_command("/goal --fresh 새 목표"),
        GoalCommandKind::FreshStart
    );
    assert_eq!(
        classify_codex_goal_command("/goal --fresh"),
        GoalCommandKind::FreshStart
    );

    // Lifecycle
    assert_eq!(
        classify_codex_goal_command("/goal pause"),
        GoalCommandKind::Lifecycle(GoalLifecycleCommand::Pause)
    );
    assert_eq!(
        classify_codex_goal_command("/goal resume"),
        GoalCommandKind::Lifecycle(GoalLifecycleCommand::Resume)
    );
    assert_eq!(
        classify_codex_goal_command("/goal clear"),
        GoalCommandKind::Lifecycle(GoalLifecycleCommand::Clear)
    );

    // NotGoal
    assert_eq!(
        classify_codex_goal_command("/goals"),
        GoalCommandKind::NotGoal
    );
    assert_eq!(
        classify_codex_goal_command("/goalkeeper"),
        GoalCommandKind::NotGoal
    );
    assert_eq!(
        classify_codex_goal_command("질문 /goal"),
        GoalCommandKind::NotGoal
    );
    assert_eq!(classify_codex_goal_command(""), GoalCommandKind::NotGoal);
}

#[test]
fn classify_codex_goal_command_for_provider_gates_non_codex() {
    // Non-Codex provider → always NotGoal
    assert_eq!(
        classify_codex_goal_command_for_provider(&ProviderKind::Claude, "/goal 새 목표", None),
        GoalCommandKind::NotGoal
    );
    // goals disabled → NotGoal
    assert_eq!(
        classify_codex_goal_command_for_provider(
            &ProviderKind::Codex,
            "/goal 새 목표",
            Some(false)
        ),
        GoalCommandKind::NotGoal
    );
    // Codex + goals enabled (or unset) → classify
    assert_eq!(
        classify_codex_goal_command_for_provider(&ProviderKind::Codex, "/goal 새 목표", Some(true)),
        GoalCommandKind::ChainedStart
    );
    assert_eq!(
        classify_codex_goal_command_for_provider(
            &ProviderKind::Codex,
            "/goal --fresh 새 목표",
            None
        ),
        GoalCommandKind::FreshStart
    );
    assert_eq!(
        classify_codex_goal_command_for_provider(&ProviderKind::Codex, "/goal pause", Some(true)),
        GoalCommandKind::Lifecycle(GoalLifecycleCommand::Pause)
    );
}

#[test]
fn codex_goal_lifecycle_notices_are_explicitly_consumed() {
    assert!(codex_goal_lifecycle_notice(GoalLifecycleCommand::Clear, false).contains("적용 완료"));
    assert!(
        codex_goal_lifecycle_notice(GoalLifecycleCommand::Clear, true).contains("현재 Codex 턴")
    );
    assert!(
        codex_goal_lifecycle_notice(GoalLifecycleCommand::Pause, false)
            .contains("Codex TUI로 전달하지 않았습니다")
    );
    assert!(
        codex_goal_lifecycle_notice(GoalLifecycleCommand::Resume, false)
            .contains("Codex TUI로 전달하지 않았습니다")
    );
}

#[test]
fn rewrite_fresh_goal_prompt_strips_fresh_marker() {
    assert_eq!(
        rewrite_fresh_goal_prompt("/goal --fresh 새 목표"),
        "/goal 새 목표"
    );
    assert_eq!(rewrite_fresh_goal_prompt("/goal --fresh"), "/goal");
    // Non-fresh prompts are returned unchanged
    assert_eq!(rewrite_fresh_goal_prompt("/goal 새 목표"), "/goal 새 목표");
}

#[test]
fn clear_resets_memento_skip_so_next_turn_can_reload_context() {
    let memento = settings::ResolvedMemorySettings {
        backend: settings::MemoryBackendKind::Memento,
        ..settings::ResolvedMemorySettings::default()
    };
    let mut session = make_session(Some("/tmp/project".to_string()), None);

    session.restore_provider_session(Some("session-1".to_string()));
    session.note_memento_context_loaded();
    assert!(
        !memento_recall_gate_decision(
            &memento,
            session.memento_context_loaded,
            "평범한 요청",
            DispatchProfile::Full,
        )
        .should_recall
    );

    session.clear_provider_session();
    assert!(
        memento_recall_gate_decision(
            &memento,
            session.memento_context_loaded,
            "평범한 요청",
            DispatchProfile::Full,
        )
        .should_recall
    );
}

#[test]
fn restored_provider_session_does_not_skip_memento_recall_until_context_reloads() {
    let memento = settings::ResolvedMemorySettings {
        backend: settings::MemoryBackendKind::Memento,
        ..settings::ResolvedMemorySettings::default()
    };
    let mut session = make_session(Some("/tmp/project".to_string()), None);

    session.restore_provider_session(Some("session-1".to_string()));
    let mut memento_context_loaded = session.memento_context_loaded;
    assert!(
        memento_recall_gate_decision(
            &memento,
            memento_context_loaded,
            "평범한 요청",
            DispatchProfile::Full,
        )
        .should_recall
    );

    session.note_memento_context_loaded();
    memento_context_loaded = session.memento_context_loaded;
    assert!(
        !memento_recall_gate_decision(
            &memento,
            memento_context_loaded,
            "평범한 요청",
            DispatchProfile::Full,
        )
        .should_recall
    );
}

#[test]
fn session_reset_reason_triggers_after_idle_timeout() {
    let mut session = make_session(Some("/tmp/project".to_string()), None);
    let last_active = tokio::time::Instant::now();
    let now = last_active + crate::services::discord::SESSION_MAX_IDLE + Duration::from_secs(1);
    session.last_active = last_active;

    assert_eq!(
        session_reset_reason_for_turn(&session, now),
        Some(SessionResetReason::IdleExpired)
    );
}

#[test]
fn session_reset_reason_triggers_after_assistant_turn_cap() {
    let mut session = make_session(Some("/tmp/project".to_string()), None);
    session.history = (0..100)
        .map(|idx| HistoryItem {
            item_type: HistoryType::Assistant,
            content: format!("assistant-{idx}"),
        })
        .collect();

    assert_eq!(
        session_reset_reason_for_turn(&session, tokio::time::Instant::now()),
        Some(SessionResetReason::AssistantTurnCap)
    );
}

#[test]
fn effective_fast_mode_channel_id_prefers_thread_parent() {
    assert_eq!(
        effective_fast_mode_channel_id(
            ChannelId::new(222),
            Some((ChannelId::new(111), Some("adk-cdx".to_string())))
        ),
        ChannelId::new(111)
    );
}

#[test]
fn effective_fast_mode_channel_id_keeps_non_thread_channel() {
    assert_eq!(
        effective_fast_mode_channel_id(ChannelId::new(222), None),
        ChannelId::new(222)
    );
}

#[test]
fn merge_reply_contexts_prefers_retry_context_first() {
    assert_eq!(
        merge_reply_contexts(
            Some("reply context".to_string()),
            Some("retry context".to_string())
        )
        .as_deref(),
        Some("retry context\n\nreply context")
    );
}

#[test]
fn parse_dispatch_context_hints_extracts_session_strategy_and_worktree() {
    let temp = tempfile::tempdir().unwrap();
    let raw = serde_json::json!({
        "worktree_path": temp.path(),
        "reset_provider_state": true,
        "recreate_tmux": true
    })
    .to_string();

    let hints = parse_dispatch_context_hints(Some(&raw), Some("review-decision"));

    assert_eq!(hints.worktree_path.as_deref(), temp.path().to_str());
    assert!(hints.stale_worktree_path.is_none());
    assert!(hints.reset_provider_state);
    assert!(hints.recreate_tmux);
}

#[test]
fn parse_dispatch_context_hints_tracks_missing_path_but_keeps_legacy_reset_flag() {
    let hints = parse_dispatch_context_hints(
        Some(r#"{"worktree_path":"/definitely/missing","force_new_session":true}"#),
        Some("review-decision"),
    );

    assert!(hints.worktree_path.is_none());
    assert_eq!(
        hints.stale_worktree_path.as_deref(),
        Some("/definitely/missing")
    );
    assert!(hints.reset_provider_state);
    assert!(!hints.recreate_tmux);
}

#[test]
fn parse_dispatch_context_hints_defaults_fresh_session_for_work_dispatches() {
    let implementation = parse_dispatch_context_hints(None, Some("implementation"));
    let review = parse_dispatch_context_hints(None, Some("review"));
    let rework = parse_dispatch_context_hints(None, Some("rework"));

    assert!(implementation.reset_provider_state);
    assert!(!implementation.recreate_tmux);
    assert!(review.reset_provider_state);
    assert!(!review.recreate_tmux);
    assert!(rework.reset_provider_state);
    assert!(!rework.recreate_tmux);
}

#[test]
fn parse_dispatch_context_hints_defaults_warm_resume_for_review_decision() {
    let hints = parse_dispatch_context_hints(None, Some("review-decision"));
    assert!(!hints.reset_provider_state);
    assert!(!hints.recreate_tmux);
}

#[test]
fn parse_dispatch_context_hints_respects_explicit_override_over_dispatch_type_default() {
    let hints =
        parse_dispatch_context_hints(Some(r#"{"force_new_session":false}"#), Some("rework"));
    assert!(!hints.reset_provider_state);
    assert!(!hints.recreate_tmux);
}

#[test]
fn parse_dispatch_context_hints_allows_tmux_recreate_without_legacy_alias() {
    let hints = parse_dispatch_context_hints(
        Some(r#"{"reset_provider_state":false,"recreate_tmux":true}"#),
        Some("review-decision"),
    );
    assert!(!hints.reset_provider_state);
    assert!(hints.recreate_tmux);
}

#[test]
fn parse_dispatch_context_hints_extracts_target_repo() {
    let hints = parse_dispatch_context_hints(
        Some(r#"{"target_repo":"/tmp/external-762","worktree_path":null}"#),
        Some("review"),
    );
    assert_eq!(hints.target_repo.as_deref(), Some("/tmp/external-762"));
    assert!(hints.worktree_path.is_none());
}

#[test]
fn parse_dispatch_context_hints_target_repo_rejects_blank_values() {
    let hints = parse_dispatch_context_hints(
        Some(r#"{"target_repo":"   ","worktree_path":null}"#),
        Some("review"),
    );
    assert!(hints.target_repo.is_none());
}

/// #762 (B): when the dispatch context pins an external `target_repo` but
/// emits `worktree_path: null` (e.g. the completion lives in repo HEAD
/// but HEAD has drifted, so refresh suppressed worktree_path per #682
/// round 3), bootstrap must land in the external repo instead of the
/// default AgentDesk workspace. Prior behavior always fell back to
/// `resolve_repo_dir()` because `DispatchContextHints` dropped
/// `target_repo` on the floor.
#[test]
fn resolve_dispatch_target_repo_dir_honors_external_target_repo_when_worktree_path_is_null() {
    // Build a real git worktree at a path that is explicitly NOT the
    // default AgentDesk workspace. `resolve_repo_dir_for_target` treats
    // absolute paths as explicit and only accepts them if the directory
    // is a valid git worktree.
    let external = tempfile::tempdir().unwrap();
    let external_dir = external.path().to_str().unwrap();
    GitCommand::new()
        .args(["init", "-b", "main"])
        .repo(external_dir)
        .run_output()
        .unwrap();
    GitCommand::new()
        .args(["config", "user.email", "test@test.com"])
        .repo(external_dir)
        .run_output()
        .unwrap();
    GitCommand::new()
        .args(["config", "user.name", "Test"])
        .repo(external_dir)
        .run_output()
        .unwrap();
    GitCommand::new()
        .args(["commit", "--allow-empty", "-m", "initial"])
        .repo(external_dir)
        .run_output()
        .unwrap();

    let raw = serde_json::json!({
        "target_repo": external_dir,
        "worktree_path": serde_json::Value::Null,
        "reviewed_commit": "0123456789abcdef0123456789abcdef01234567",
    })
    .to_string();
    let hints = parse_dispatch_context_hints(Some(&raw), Some("review"));

    assert_eq!(hints.target_repo.as_deref(), Some(external_dir));
    assert!(
        hints.worktree_path.is_none(),
        "null worktree_path must not be synthesized from target_repo by the hints parser"
    );

    // This is the specific regression: bootstrap must resolve to the
    // external repo, NOT the default AgentDesk workspace. Prior code
    // called `resolve_repo_dir()` unconditionally when `worktree_path`
    // was absent.
    let resolved = resolve_dispatch_target_repo_dir(hints.target_repo.as_deref())
        .expect("external target_repo with null worktree_path must resolve to the repo dir");
    assert_eq!(
        std::fs::canonicalize(&resolved).unwrap(),
        std::fs::canonicalize(external_dir).unwrap()
    );
}

#[test]
fn resolve_dispatch_target_repo_dir_returns_none_for_missing_target_repo() {
    assert!(resolve_dispatch_target_repo_dir(None).is_none());
    assert!(resolve_dispatch_target_repo_dir(Some("")).is_none());
    assert!(resolve_dispatch_target_repo_dir(Some("   ")).is_none());
}

#[test]
fn resolve_dispatch_target_repo_dir_rejects_nonexistent_path() {
    // A target_repo that references a path outside any configured
    // mapping cannot be resolved — bootstrap falls back to the default
    // workspace, not to the (nonexistent) requested path.
    assert!(
        resolve_dispatch_target_repo_dir(Some("/tmp/agentdesk-issue-762-definitely-not-a-repo"))
            .is_none()
    );
}

#[test]
fn session_runtime_state_after_redirect_prefers_reused_thread_state() {
    let parent_dir = tempfile::tempdir().unwrap();
    let thread_dir = tempfile::tempdir().unwrap();
    let parent_channel_id = ChannelId::new(100);
    let thread_channel_id = ChannelId::new(200);

    let mut sessions = std::collections::HashMap::new();
    let mut parent = make_session(Some(parent_dir.path().to_str().unwrap().to_string()), None);
    parent.restore_provider_session(Some("parent-session".to_string()));
    sessions.insert(parent_channel_id, parent);

    let thread = make_session(Some(thread_dir.path().to_str().unwrap().to_string()), None);
    sessions.insert(thread_channel_id, thread);

    let resolved = session_runtime_state_after_redirect(
        &mut sessions,
        parent_channel_id,
        thread_channel_id,
        (
            Some("parent-session".to_string()),
            true,
            parent_dir.path().to_str().unwrap().to_string(),
        ),
    );

    assert_eq!(resolved.0, None);
    assert!(!resolved.1);
    assert_eq!(resolved.2, thread_dir.path().to_str().unwrap());
}

/// #762 round-2 (B): reused threads that bypass `bootstrap_thread_session`
/// still need their session CWD refreshed whenever the new dispatch
/// points at a different effective path — even when no `worktree_path`
/// is supplied. Prior behavior only updated session.current_path when
/// `dispatch_worktree_path.is_some()`, so external-repo reviews that
/// emitted only `target_repo` quietly executed inside the previous
/// implementation's repo.
#[test]
fn dispatch_session_path_should_update_when_target_repo_diverges_without_worktree() {
    // Reused thread: dispatch present, no worktree_path, but
    // target_repo resolved to a different directory than the
    // session's stale current_path. Must update.
    assert!(
        dispatch_session_path_should_update(
            true, // has_dispatch
            Some("review"),
            false, // has_worktree_path
            false, // existing thread, no fresh bootstrap this turn
            "/tmp/stale-impl-repo",
            "/tmp/external-target-repo",
        ),
        "reused thread with divergent target_repo must update session CWD"
    );
}

#[test]
fn dispatch_session_path_should_update_still_triggers_for_worktree_path_dispatch() {
    // Classic #259 path: dispatch has worktree_path. Always update,
    // even when stale current_path already happens to match.
    assert!(
        dispatch_session_path_should_update(
            true,
            Some("review"),
            true,
            false,
            "/tmp/impl-wt",
            "/tmp/impl-wt",
        ),
        "worktree_path dispatches must always update session CWD"
    );
    assert!(
        dispatch_session_path_should_update(
            true,
            Some("review"),
            true,
            false,
            "/tmp/stale",
            "/tmp/fresh-wt",
        ),
        "worktree_path dispatches with divergent path must update"
    );
}

#[test]
fn dispatch_session_path_should_update_skips_when_paths_match() {
    // No dispatch → leave alone.
    assert!(!dispatch_session_path_should_update(
        false, None, false, false, "/tmp/a", "/tmp/b",
    ));
    // Dispatch present but worktree_path absent AND effective path
    // matches current path → nothing to update.
    assert!(!dispatch_session_path_should_update(
        true,
        Some("review"),
        false,
        false,
        "/tmp/same",
        "/tmp/same",
    ));
}

#[test]
fn dispatch_session_path_should_update_fresh_bootstrap_for_worktree_dispatch() {
    assert!(dispatch_session_path_should_update(
        true,
        Some("implementation"),
        true,
        true,
        "/tmp/workspaces/agentdesk",
        "/tmp/worktrees/dispatch-934",
    ));
}

#[test]
fn evaluate_dispatch_cwd_policy_rejects_main_workspace_for_implementation() {
    let root = tempfile::tempdir().unwrap();
    let main_workspace = root.path().join("workspaces").join("agentdesk");
    let worktrees_root = root.path().join("worktrees");
    std::fs::create_dir_all(&main_workspace).unwrap();
    std::fs::create_dir_all(worktrees_root.join("impl-934")).unwrap();

    let decision = evaluate_dispatch_cwd_policy(
        Some("implementation"),
        main_workspace.to_str().unwrap(),
        Some(main_workspace.as_path()),
        Some(worktrees_root.as_path()),
    );

    assert!(decision.log_main_workspace_error);
    assert!(decision.reject_for_missing_fresh_worktree);
}

#[test]
fn evaluate_dispatch_cwd_policy_allows_review_repo_root_fallback() {
    let root = tempfile::tempdir().unwrap();
    let main_workspace = root.path().join("workspaces").join("agentdesk");
    let external_repo = root.path().join("external-review");
    let worktrees_root = root.path().join("worktrees");
    std::fs::create_dir_all(&main_workspace).unwrap();
    std::fs::create_dir_all(&external_repo).unwrap();
    std::fs::create_dir_all(&worktrees_root).unwrap();

    let decision = evaluate_dispatch_cwd_policy(
        Some("review"),
        external_repo.to_str().unwrap(),
        Some(main_workspace.as_path()),
        Some(worktrees_root.as_path()),
    );

    assert!(!decision.log_main_workspace_error);
    assert!(!decision.reject_for_missing_fresh_worktree);
}

#[test]
fn session_runtime_state_after_redirect_keeps_original_state_when_channel_unchanged() {
    let channel_id = ChannelId::new(100);
    let dir = tempfile::tempdir().unwrap();
    let original = (
        Some("session-1".to_string()),
        true,
        dir.path().to_str().unwrap().to_string(),
    );

    let resolved = session_runtime_state_after_redirect(
        &mut std::collections::HashMap::new(),
        channel_id,
        channel_id,
        original.clone(),
    );

    assert_eq!(resolved, original);
}

#[test]
fn race_requeue_preserves_reply_boundary_without_reply_context() {
    let queued = build_race_requeued_intervention(
        UserId::new(7),
        MessageId::new(8),
        "hello",
        None,
        true,
        true,
        Vec::new(),
        None,
    );

    assert!(queued.has_reply_boundary);
    assert!(queued.reply_context.is_none());
    assert!(queued.merge_consecutive);
    assert!(queued.voice_announcement.is_none());
}

#[test]
fn race_requeue_preserves_non_mergeable_turns() {
    let queued = build_race_requeued_intervention(
        UserId::new(7),
        MessageId::new(8),
        "hello",
        None,
        false,
        false,
        Vec::new(),
        None,
    );

    assert!(!queued.has_reply_boundary);
    assert!(!queued.merge_consecutive);
    assert!(queued.voice_announcement.is_none());
}

// #2266: when a voice-transcript announcement loses the
// `mailbox_try_start_turn` race, the queued `Intervention` must carry
// the full `VoiceTranscriptAnnouncement` payload so the dispatch path
// can reinsert it into the per-process store before re-entering
// `handle_text_message`. Without this the dispatch path sees the entry
// missing (already taken by the active turn) and degrades to plain text.
#[test]
fn race_requeue_carries_voice_announcement_payload() {
    let announcement = crate::voice::prompt::VoiceTranscriptAnnouncement {
        transcript: "상태 알려줘".to_string(),
        user_id: "42".to_string(),
        utterance_id: "utt-2266".to_string(),
        language: "ko-KR".to_string(),
        verbose_progress: true,
        started_at: Some("2026-05-16T10:00:00+09:00".to_string()),
        completed_at: Some("2026-05-16T10:00:01+09:00".to_string()),
        samples_written: Some(48_000),
        control_channel_id: None,
        stt_mode: None,
        stt_latency_ms: None,
    };
    let queued = build_race_requeued_intervention(
        UserId::new(7),
        MessageId::new(8),
        "상태 알려줘",
        None,
        false,
        false,
        Vec::new(),
        Some(announcement.clone()),
    );

    let carried = queued
        .voice_announcement
        .as_ref()
        .expect("voice announcement must be carried through the queued intervention");
    assert_eq!(carried.utterance_id, "utt-2266");
    assert_eq!(carried.transcript, "상태 알려줘");
    assert_eq!(carried.language, "ko-KR");
    assert!(carried.verbose_progress);
    assert_eq!(carried.samples_written, Some(48_000));
    assert_eq!(*carried, announcement);
}

// #2266: simulate the busy-channel timeline end-to-end at the
// mailbox/announce-meta seam:
//   1. The active `handle_text_message` consumes the announce-meta
//      store entry (line ~2261).
//   2. `mailbox_try_start_turn` returns false → the queued
//      `Intervention` is built via `build_race_requeued_intervention`
//      with the in-memory announcement payload carried through.
//   3. The dispatch path (which would re-enter `handle_text_message`)
//      reinserts the announcement into the store keyed by the queued
//      `intervention.message_id`.
//   4. The next `handle_text_message` `take()` recovers the full voice
//      transcript framing instead of degrading to plain text.
#[test]
fn busy_channel_queued_voice_announcement_is_restored_for_dispatch() {
    let user_msg_id = poise::serenity_prelude::MessageId::new(2_266_001);
    let announcement = crate::voice::prompt::VoiceTranscriptAnnouncement {
        transcript: "회의록 정리해줘".to_string(),
        user_id: "555".to_string(),
        utterance_id: "utt-busy-race".to_string(),
        language: "ko-KR".to_string(),
        verbose_progress: false,
        started_at: None,
        completed_at: None,
        samples_written: None,
        control_channel_id: None,
        stt_mode: None,
        stt_latency_ms: None,
    };

    // Step 1: active turn consumes the store entry (mirroring
    // `handle_text_message` line ~2261).
    let store = crate::voice::announce_meta::VoiceAnnouncementMetaStore::default();
    store.insert(user_msg_id, announcement.clone());
    let active_take = store
        .take(user_msg_id)
        .expect("active turn must consume the announcement first");
    assert_eq!(active_take.utterance_id, "utt-busy-race");
    assert!(
        store.take(user_msg_id).is_none(),
        "store entry must be drained after the active take()"
    );

    // Step 2: mailbox_try_start_turn==false → race-loss enqueue carries
    // the announcement through the Intervention payload.
    let queued = build_race_requeued_intervention(
        UserId::new(555),
        user_msg_id,
        "회의록 정리해줘",
        None,
        false,
        false,
        Vec::new(),
        Some(active_take.clone()),
    );
    assert!(queued.voice_announcement.is_some());

    // Step 3: dispatch path reinserts before re-entering
    // handle_text_message. (The production hook lives in
    // `gateway::dispatch_queued_turn` and writes to the global store;
    // here we drive the same store directly to validate the contract.)
    if let Some(payload) = queued.voice_announcement.as_ref() {
        store.insert(queued.message_id, payload.clone());
    }

    // Step 4: dispatched handle_text_message recovers the full payload.
    let dispatched = store
        .take(queued.message_id)
        .expect("dispatched take() must recover the voice announcement");
    assert_eq!(dispatched, announcement);
}

// #2266 (Codex round-2 finding [high] — live queued dispatch must
// re-authorize the embedded voice payload against the announce bot,
// not against the previous turn's owner):
//   - The race-loss enqueue path stamps `Intervention.author_id` with
//     the ORIGINAL Discord author (the announce bot for voice transcripts)
//     rather than the post-rebind voice-user id, so the subsequent
//     queued dispatch can replay the same announce_bot authorization
//     check at line ~2274 of `handle_text_message`.
//   - This regression locks in the contract: a queued voice
//     `Intervention` carries `author_id == announce_bot_user_id`.
#[test]
fn race_requeue_attributes_voice_intervention_to_announce_bot() {
    let announce_bot_id = UserId::new(999_111);
    let voice_user_id = UserId::new(42);
    let user_msg_id = poise::serenity_prelude::MessageId::new(2_266_007);
    let announcement = crate::voice::prompt::VoiceTranscriptAnnouncement {
        transcript: "회의록 정리해줘".to_string(),
        user_id: voice_user_id.get().to_string(),
        utterance_id: "utt-author".to_string(),
        language: "ko-KR".to_string(),
        verbose_progress: false,
        started_at: None,
        completed_at: None,
        samples_written: None,
        control_channel_id: None,
        stt_mode: None,
        stt_latency_ms: None,
    };

    // The race-loss enqueue path uses `original_request_owner`, which is
    // the Discord author of the raw message (the announce bot), NOT the
    // voice-user id that `handle_text_message` rebinds to for the rest
    // of the active-turn flow.
    let queued = build_race_requeued_intervention(
        announce_bot_id,
        user_msg_id,
        &announcement.transcript,
        None,
        false,
        false,
        Vec::new(),
        Some(announcement.clone()),
    );

    assert_eq!(
        queued.author_id, announce_bot_id,
        "queued voice intervention must be attributed to the announce bot so the\n             dispatch path's authorization check `announce_bot_id == Some(request_owner)`\n             still passes when the embedded payload is reinserted",
    );
    assert_ne!(
        queued.author_id, voice_user_id,
        "queued voice intervention author_id must NOT be the voice-user id,\n             which would make handle_text_message treat the embedded announcement\n             as spoofed and discard it",
    );
}

// #2266 (Codex finding [high] — intake-gate must not consume the store):
// the intake-gate path peeks the announce_meta store via peek_clone so
// the active dispatch path still finds the entry. After embedding the
// payload in the queued Intervention, the original store entry must
// still be readable for the active handle_text_message take().
#[test]
fn intake_gate_peek_clone_does_not_consume_store_entry() {
    let user_msg_id = poise::serenity_prelude::MessageId::new(2_266_002);
    let announcement = crate::voice::prompt::VoiceTranscriptAnnouncement {
        transcript: "hello".to_string(),
        user_id: "1".to_string(),
        utterance_id: "utt-peek".to_string(),
        language: "en-US".to_string(),
        verbose_progress: false,
        started_at: None,
        completed_at: None,
        samples_written: None,
        control_channel_id: None,
        stt_mode: None,
        stt_latency_ms: None,
    };

    let store = crate::voice::announce_meta::VoiceAnnouncementMetaStore::default();
    store.insert(user_msg_id, announcement.clone());

    // Intake-gate snapshot via peek_clone for the queued Intervention.
    let peeked = store
        .peek_clone(user_msg_id)
        .expect("peek_clone must return the stored announcement");
    assert_eq!(peeked, announcement);

    // After peek, the active dispatch path's take() must still succeed.
    let active = store
        .take(user_msg_id)
        .expect("peek_clone must not consume the entry");
    assert_eq!(active, announcement);
    // And the next take() (e.g. the queued dispatch path before
    // reinsert) reports None — confirming peek/take semantics are
    // intact.
    assert!(store.take(user_msg_id).is_none());
}

// #2266 (Codex finding [high] — durable on-disk queue must round-trip
// the voice metadata): serialize an Intervention through the
// PendingQueueItem-derived JSON shape with the announcement embedded,
// then restore via `pending_queue_item_to_intervention` and verify the
// payload survives. Covers the post-restart hydrate timeline where
// the in-memory store has already been wiped.
#[test]
fn durable_queue_round_trips_voice_announcement_for_restart() {
    let announcement = crate::voice::prompt::VoiceTranscriptAnnouncement {
        transcript: "회의록 정리해줘".to_string(),
        user_id: "555".to_string(),
        utterance_id: "utt-durable".to_string(),
        language: "ko-KR".to_string(),
        verbose_progress: true,
        started_at: Some("2026-05-16T10:00:00+09:00".to_string()),
        completed_at: Some("2026-05-16T10:00:01+09:00".to_string()),
        samples_written: Some(48_000),
        control_channel_id: None,
        stt_mode: None,
        stt_latency_ms: None,
    };
    let item = crate::services::turn_orchestrator::PendingQueueItem {
        author_id: 555,
        author_is_bot: false,
        message_id: 2_266_003,
        source_message_ids: vec![2_266_003],
        text: "회의록 정리해줘".to_string(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        channel_id: Some(42),
        channel_name: None,
        override_channel_id: None,
        voice_announcement: Some(announcement.clone()),
    };

    // Round-trip through JSON to mirror the on-disk format.
    let json = serde_json::to_string(&item).expect("serialize");
    let restored: crate::services::turn_orchestrator::PendingQueueItem =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.voice_announcement.as_ref(), Some(&announcement));

    // Older queue files (no voice_announcement field) must still load.
    let legacy_json = serde_json::json!({
        "author_id": 1,
        "message_id": 2,
        "source_message_ids": [2u64],
        "text": "plain",
        "reply_context": null,
        "has_reply_boundary": false,
        "merge_consecutive": false,
    })
    .to_string();
    let legacy: crate::services::turn_orchestrator::PendingQueueItem =
        serde_json::from_str(&legacy_json).expect("legacy deserialize");
    assert!(legacy.voice_announcement.is_none());
}

#[test]
fn build_system_discord_context_omits_user_identity() {
    let context = build_system_discord_context(
        Some("adk-cdx"),
        Some("agentdesk"),
        ChannelId::new(42),
        false,
    );

    assert_eq!(
        context,
        "Discord context: channel #adk-cdx (ID: 42) (category: agentdesk)"
    );
    assert!(!context.contains("user:"));
    assert!(!context.contains("author_id"));
}

#[test]
fn wrap_user_prompt_with_author_adds_user_prefix() {
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
fn dm_channel_roster_keeps_single_requester() {
    let shared = super::super::super::make_shared_data_for_tests();
    let channel_id = ChannelId::new(42);
    shared.record_channel_speaker(channel_id, UserId::new(101), "Alice", false);
    shared.record_channel_speaker(channel_id, UserId::new(202), "Bob", false);
    shared.record_channel_speaker(channel_id, UserId::new(101), "Alice", true);

    let roster = shared.channel_roster(channel_id, UserId::new(999), "Fallback");
    assert_eq!(roster, vec![UserRecord::new(UserId::new(101), "Alice")]);
}

#[test]
fn watchdog_prealert_helpers_parse_and_dedupe_deadline() {
    assert_eq!(watchdog_deadlock_prealert_bot_name(), "announce");
    assert_eq!(
        parse_watchdog_alert_channel_id("channel:<#12345>"),
        Some(ChannelId::new(12345))
    );
    assert_eq!(
        parse_watchdog_alert_channel_id("67890"),
        Some(ChannelId::new(67890))
    );
    assert_eq!(parse_watchdog_alert_channel_id("deadlock-manager"), None);

    let deadline = 1_000_000;
    assert!(!should_send_watchdog_deadlock_prealert(
        deadline - WATCHDOG_DEADLOCK_PREALERT_MS - 1,
        deadline,
        None
    ));
    assert!(should_send_watchdog_deadlock_prealert(
        deadline - WATCHDOG_DEADLOCK_PREALERT_MS,
        deadline,
        None
    ));
    assert!(!should_send_watchdog_deadlock_prealert(
        deadline - 1,
        deadline,
        Some(deadline)
    ));
    assert!(!should_send_watchdog_deadlock_prealert(
        deadline, deadline, None
    ));
}

#[test]
fn watchdog_prealert_message_contains_extension_contract() {
    let now = 60 * 60 * 1000;
    let deadline = now + 4 * 60 * 1000;
    let started = 0;
    let max_deadline = started + 3 * 60 * 60 * 1000;

    let message = build_watchdog_deadlock_prealert_message(
        &ProviderKind::Codex,
        ChannelId::new(42),
        now,
        deadline,
        started,
        max_deadline,
        None,
    );

    assert!(message.contains("[Watchdog pre-timeout]"));
    assert!(message.contains("channel_id: 42"));
    assert!(message.contains("provider: codex"));
    assert!(message.contains("remaining: 4분"));
    assert!(message.contains("POST /api/turns/42/extend-timeout"));
}

#[test]
fn watchdog_deadline_extension_moves_deadline_and_tracked_max() {
    let token = CancelToken::new();
    token
        .watchdog_deadline_ms
        .store(1_000, std::sync::atomic::Ordering::Relaxed);
    token
        .watchdog_max_deadline_ms
        .store(2_000, std::sync::atomic::Ordering::Relaxed);
    let extension = crate::services::turn_orchestrator::WatchdogDeadlineExtension {
        requested_deadline_ms: 4_000,
        new_deadline_ms: 4_000,
        max_deadline_ms: 4_000,
        applied_extend_secs: 2,
        requested_extend_secs: 2,
        extension_count: 1,
        extension_count_limit: u32::MAX,
        extension_total_secs: 2,
        extension_total_secs_limit: u64::MAX,
        clamped: false,
    };

    assert_eq!(apply_watchdog_deadline_extension(&token, extension), 4_000);
    assert_eq!(
        token
            .watchdog_deadline_ms
            .load(std::sync::atomic::Ordering::Relaxed),
        4_000
    );
    assert_eq!(
        token
            .watchdog_max_deadline_ms
            .load(std::sync::atomic::Ordering::Relaxed),
        4_000
    );
}

#[test]
fn attach_paused_turn_watcher_pauses_existing_tmux_owner_channel() {
    let shared = super::super::super::make_shared_data_for_tests();
    let owner_channel = ChannelId::new(1485506232256168136);
    let thread_channel = ChannelId::new(1485506232256168137);
    let tmux_name = "AgentDesk-codex-adk-cdx-owner".to_string();
    let output_path = "/tmp/agentdesk-test-owner-output.jsonl".to_string();
    let owner_paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let owner_pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    shared.tmux_watchers.insert(
        owner_channel,
        TmuxWatcherHandle {
            tmux_session_name: tmux_name.clone(),
            output_path: output_path.clone(),
            paused: owner_paused.clone(),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: owner_pause_epoch.clone(),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                super::super::super::tmux_watcher_now_ms(),
            )),
            mailbox_finalize_owed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    );

    let owner = attach_paused_turn_watcher(
        &shared,
        Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
        &ProviderKind::Codex,
        thread_channel,
        Some(tmux_name),
        Some(output_path),
        0,
        "unit-test-turn-start",
    );

    assert_eq!(owner, owner_channel);
    assert!(
        owner_paused.load(std::sync::atomic::Ordering::Relaxed),
        "turn start must pause the live owner watcher, not the requested thread slot"
    );
    assert_eq!(
        owner_pause_epoch.load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert!(
        !shared.tmux_watchers.contains_key(&thread_channel),
        "reusing an owner watcher must not install a duplicate thread watcher"
    );
}

#[test]
fn attach_paused_turn_watcher_skips_prelaunch_dead_tmux() {
    let shared = super::super::super::make_shared_data_for_tests();
    let channel = ChannelId::new(1485506232256168138);
    let owner = attach_paused_turn_watcher(
        &shared,
        Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
        &ProviderKind::Codex,
        channel,
        Some("AgentDesk-codex-not-yet-spawned".to_string()),
        Some("/tmp/agentdesk-test-output.jsonl".to_string()),
        0,
        "unit-test-prelaunch",
    );

    assert_eq!(owner, channel);
    assert!(
        !shared.tmux_watchers.contains_key(&channel),
        "prelaunch turn start must wait for TmuxReady instead of spawning a watcher that immediately observes a dead pane"
    );
}

#[test]
fn multi_user_turns_keep_system_prompt_identical() {
    let discord_context = build_system_discord_context(
        Some("multi-user"),
        Some("agentdesk"),
        ChannelId::new(9001),
        false,
    );

    let alice_system = prompt_builder::build_system_prompt(
        &discord_context,
        &[],
        "/tmp/work",
        ChannelId::new(9001),
        "token",
        None,
        false,
        prompt_builder::DispatchProfile::Full,
        None,
        None,
        None,
        None,
        None,
        false,
    );
    let bob_system = prompt_builder::build_system_prompt(
        &discord_context,
        &[],
        "/tmp/work",
        ChannelId::new(9001),
        "token",
        None,
        false,
        prompt_builder::DispatchProfile::Full,
        None,
        None,
        None,
        None,
        None,
        false,
    );

    assert_eq!(alice_system.as_bytes(), bob_system.as_bytes());

    let alice_user_prompt =
        wrap_user_prompt_with_author("Alice", UserId::new(101), "same task".to_string());
    let bob_user_prompt =
        wrap_user_prompt_with_author("Bob", UserId::new(202), "same task".to_string());

    assert!(alice_user_prompt.starts_with("[User: Alice (ID: 101)]"));
    assert!(bob_user_prompt.starts_with("[User: Bob (ID: 202)]"));
    assert_ne!(alice_user_prompt, bob_user_prompt);
}

/// codex review round-8 P2 (#1332): when `send_intake_placeholder` POSTs
/// while another concurrent message has lost the race and queued itself,
/// the failure-path mailbox release MUST schedule a deferred kickoff so
/// the queued message is dispatched. The previous code ignored
/// `FinishTurnResult::has_pending` and let the channel sit idle with a
/// persisted queued item, so this test pins the kickoff.
#[tokio::test(flavor = "current_thread")]
async fn release_mailbox_after_placeholder_post_failure_schedules_kickoff_when_pending() {
    use crate::services::provider::CancelToken;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    let shared = super::super::super::make_shared_data_for_tests();
    let provider = super::super::super::ProviderKind::Codex;
    let channel_id = ChannelId::new(987_654_321);
    let owner = UserId::new(42);
    let active_msg_id = MessageId::new(1_000);
    let queued_msg_id = MessageId::new(1_001);

    // 1. Active turn acquires the slot via the start-turn race.
    let cancel_token = Arc::new(CancelToken::new());
    let started = super::super::super::mailbox_try_start_turn(
        shared.as_ref(),
        channel_id,
        cancel_token.clone(),
        owner,
        active_msg_id,
    )
    .await;
    assert!(started, "fresh mailbox should accept the active turn");
    shared.global_active.fetch_add(1, Ordering::Relaxed);

    // 2. While the placeholder POST is in flight, a concurrent message
    //    loses the race and is enqueued as a soft intervention.
    let enqueue = super::super::super::mailbox_enqueue_intervention(
        shared.as_ref(),
        &provider,
        channel_id,
        super::super::super::Intervention {
            author_id: owner,
            author_is_bot: false,
            message_id: queued_msg_id,
            source_message_ids: vec![queued_msg_id],
            text: "race-loser queued message".to_string(),
            mode: super::super::super::InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        },
    )
    .await;
    assert!(enqueue.enqueued, "concurrent race-loser should enqueue");

    // 3. Snapshot the deferred-hook backlog BEFORE the simulated failure
    //    so we can prove the kickoff was actually scheduled.
    let backlog_before = shared.deferred_hook_backlog.load(Ordering::Relaxed);

    // 4. Simulate the placeholder POST failure: invoke the new release
    //    helper that wraps `mailbox_finish_turn` + the deferred kickoff.
    let kicked =
        release_mailbox_after_placeholder_post_failure(&shared, &provider, channel_id).await;

    // 5. The helper MUST report a kickoff was scheduled, the deferred
    //    backlog MUST have been incremented synchronously by
    //    `schedule_deferred_idle_queue_kickoff`, and the mailbox MUST
    //    still have the queued item ready for the kickoff to drain.
    assert!(kicked, "kickoff must be scheduled when has_pending == true");
    let backlog_after = shared.deferred_hook_backlog.load(Ordering::Relaxed);
    assert_eq!(
        backlog_after,
        backlog_before + 1,
        "deferred_hook_backlog must increment exactly once when a kickoff is scheduled (channel must not be left idle with a queued item)"
    );

    let snapshot = shared.mailbox(channel_id).snapshot().await;
    assert_eq!(
        snapshot.intervention_queue.len(),
        1,
        "queued race-loser must remain in the mailbox so the deferred kickoff can drain it"
    );
    assert_eq!(
        snapshot.intervention_queue[0].message_id, queued_msg_id,
        "queued message identity must be preserved across mailbox_finish_turn"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn busy_pre_submit_requeues_and_schedules_idle_kickoff_when_pending() {
    use crate::services::provider::CancelToken;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    let shared = super::super::super::make_shared_data_for_tests();
    let provider = super::super::super::ProviderKind::Claude;
    let channel_id = ChannelId::new(887_766_554);
    let owner = UserId::new(44);
    let active_msg_id = MessageId::new(2_500);

    let cancel_token = Arc::new(CancelToken::new());
    let started = super::super::super::mailbox_try_start_turn(
        shared.as_ref(),
        channel_id,
        cancel_token,
        owner,
        active_msg_id,
    )
    .await;
    assert!(started, "fresh mailbox should accept the active turn");
    shared.global_active.fetch_add(1, Ordering::Relaxed);

    let enqueue = enqueue_busy_tui_followup_for_retry(
        &shared,
        &provider,
        channel_id,
        owner,
        active_msg_id,
        "queued after transcript still streaming",
        None,
        false,
        false,
        Vec::new(),
        None,
    )
    .await;
    assert!(
        enqueue.enqueued,
        "busy pre-submit handling must queue the current message instead of dropping it"
    );

    let backlog_before = shared.deferred_hook_backlog.load(Ordering::Relaxed);
    let kicked =
        release_mailbox_after_hosted_tui_busy_pre_submit(&shared, &provider, channel_id).await;

    assert!(
        kicked,
        "hosted TUI busy pre-submit must schedule a deferred kickoff when it leaves a queued item behind"
    );
    assert_eq!(
        shared.deferred_hook_backlog.load(Ordering::Relaxed),
        backlog_before + 1,
        "hosted TUI busy pre-submit must not leave an idle mailbox with a queued retry"
    );

    let snapshot = shared.mailbox(channel_id).snapshot().await;
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(snapshot.intervention_queue[0].message_id, active_msg_id);
    assert_eq!(
        snapshot.intervention_queue[0].text,
        "queued after transcript still streaming"
    );
}

/// Negative: when the mailbox queue is empty after `mailbox_finish_turn`,
/// the failure-path helper must NOT schedule a deferred kickoff (no
/// double-kicks, no spurious wake-ups for channels with nothing pending).
#[tokio::test(flavor = "current_thread")]
async fn release_mailbox_after_placeholder_post_failure_skips_kickoff_when_idle() {
    use crate::services::provider::CancelToken;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    let shared = super::super::super::make_shared_data_for_tests();
    let provider = super::super::super::ProviderKind::Codex;
    let channel_id = ChannelId::new(123_456_789);
    let owner = UserId::new(7);
    let active_msg_id = MessageId::new(2_000);

    let cancel_token = Arc::new(CancelToken::new());
    let started = super::super::super::mailbox_try_start_turn(
        shared.as_ref(),
        channel_id,
        cancel_token.clone(),
        owner,
        active_msg_id,
    )
    .await;
    assert!(started, "fresh mailbox should accept the active turn");
    shared.global_active.fetch_add(1, Ordering::Relaxed);

    let backlog_before = shared.deferred_hook_backlog.load(Ordering::Relaxed);
    let kicked =
        release_mailbox_after_placeholder_post_failure(&shared, &provider, channel_id).await;
    assert!(
        !kicked,
        "no kickoff should be scheduled when nothing is pending"
    );
    let backlog_after = shared.deferred_hook_backlog.load(Ordering::Relaxed);
    assert_eq!(
        backlog_after, backlog_before,
        "deferred_hook_backlog must not grow when the queue is empty (avoid spurious wake-ups)"
    );
}

/// codex review round-9 P2 (#1332): when a dispatch-role-routed message
/// loses the mailbox start-turn race, the new race-loss path enqueues
/// the intervention BEFORE awaiting any Discord HTTP. This test
/// simulates the round-8-finding race directly:
///
///   1. Active turn is running.
///   2. `dispatch_role_overrides[channel] = override_channel` is
///      installed (pretend this turn was a Codex-review hand-off
///      pinning a sister channel).
///   3. A new message arrives, loses the race, and goes through the
///      round-9 ordering — **enqueue first, then POST placeholder**.
///   4. **DURING the simulated POST await window**, the active turn
///      finishes (`mailbox_finish_turn`).
///   5. `turn_bridge` mirror logic checks `finish.has_pending` —
///      because we already enqueued, `has_pending == true`, so the
///      override is preserved. The queued dispatch will run under the
///      intended dispatch routing.
///
/// Pre round-9 (enqueue AFTER the POST await): the active turn would
/// finalize before our enqueue, observe `has_pending == false`, and
/// `turn_bridge` would clear `dispatch_role_overrides`. Our late
/// enqueue would then be persisted/routed without the override and the
/// queued dispatch would silently run under the wrong provider.
#[tokio::test(flavor = "current_thread")]
async fn race_loss_enqueue_before_post_preserves_dispatch_role_overrides() {
    use crate::services::provider::CancelToken;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    let shared = super::super::super::make_shared_data_for_tests();
    let provider = super::super::super::ProviderKind::Claude;
    let channel_id = ChannelId::new(987_654_321);
    let override_channel = ChannelId::new(111_222_333);
    let owner = UserId::new(11);
    let active_user_msg_id = MessageId::new(5_000);
    let race_lost_msg_id = MessageId::new(5_001);

    // (1) Active turn running.
    let active_token = Arc::new(CancelToken::new());
    let started = super::super::super::mailbox_try_start_turn(
        shared.as_ref(),
        channel_id,
        active_token.clone(),
        owner,
        active_user_msg_id,
    )
    .await;
    assert!(started, "fresh mailbox must accept the first turn");
    shared.global_active.fetch_add(1, Ordering::Relaxed);

    // (2) Dispatch hand-off override installed for this channel.
    shared
        .dispatch_role_overrides
        .insert(channel_id, override_channel);
    assert!(
        shared.dispatch_role_overrides.contains_key(&channel_id),
        "override must be present at the start of the race"
    );

    // (3) Round-9 ordering: race-loss enqueues the intervention BEFORE
    // any Discord HTTP await. (The actual POST is omitted from the
    // unit test — what matters is the ordering relative to
    // `mailbox_finish_turn` of the still-active turn.)
    let race_lost_msg_id_clone = race_lost_msg_id;
    let outcome = super::super::super::mailbox_enqueue_intervention(
        shared.as_ref(),
        &provider,
        channel_id,
        super::super::super::Intervention {
            author_id: owner,
            author_is_bot: false,
            message_id: race_lost_msg_id_clone,
            source_message_ids: vec![race_lost_msg_id_clone],
            text: "queued during race".to_string(),
            mode: super::super::super::InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        },
    )
    .await;
    assert!(outcome.enqueued, "race-loss intervention must enqueue");

    // (4) Simulated active-turn finalization that, in the live system,
    // would happen during the placeholder POST await window. Mirror
    // the turn_bridge logic: if `has_pending == false`, clear the
    // override; otherwise keep it.
    let finish =
        super::super::super::mailbox_finish_turn(shared.as_ref(), &provider, channel_id).await;
    assert!(
        finish.removed_token.is_some(),
        "finish_turn should remove the active turn's cancel token"
    );
    assert!(
        finish.has_pending,
        "the queued intervention must surface as pending so turn_bridge keeps the override"
    );
    if !finish.has_pending {
        // Mirrors `turn_bridge` (see src/services/discord/turn_bridge/mod.rs:2136):
        // `if !finish.has_pending { dispatch_role_overrides.remove(&channel_id); }`
        shared.dispatch_role_overrides.remove(&channel_id);
    }

    // (5) Override survives, ready for the queued dispatch to use.
    assert!(
        shared.dispatch_role_overrides.contains_key(&channel_id),
        "round-9: enqueueing before the POST await preserves dispatch_role_overrides across active-turn finalization"
    );
    assert_eq!(
        shared
            .dispatch_role_overrides
            .get(&channel_id)
            .map(|entry| *entry),
        Some(override_channel),
        "the override channel must still resolve to the intended dispatch routing"
    );

    // The queued intervention must still be in the mailbox so the
    // subsequent kickoff can dispatch it under the preserved override.
    let snapshot = super::super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;
    assert!(snapshot.cancel_token.is_none(), "active turn must be done");
    assert_eq!(
        snapshot.intervention_queue.len(),
        1,
        "the race-lost intervention must remain queued"
    );
    assert_eq!(
        snapshot.intervention_queue[0].message_id, race_lost_msg_id,
        "queued head must be our race-lost message"
    );
}

/// codex review round-10 P2 (#1332): the round-9 race-loss path
/// snapshotted `mailbox.active_user_message_id` BEFORE acquiring the
/// per-channel `queued_placeholders_persist_lock`. The residual race:
/// if the active turn finishes between the snapshot and the lock
/// acquire, the dispatch path can dequeue our just-enqueued
/// intervention, take the lock, see no mapping, post a fresh Active
/// placeholder, release the lock — and THIS branch then takes the
/// lock with a stale snapshot result, inserts a Queued mapping for a
/// turn that is already running, and renders a stale `📬` card +
/// sidecar entry that no future event will reference.
///
/// Round-10 fix: take the per-channel persist lock FIRST, then
/// snapshot the mailbox UNDER the lock. `dispatch_queued_turn`'s
/// `remove_queued_placeholder` mutator also serializes through the
/// same per-channel mutex, so once we hold the guard the dispatch
/// path cannot promote our intervention to active until we release.
///
/// This test simulates the "active turn finishes between our former
/// snapshot-spot and lock-acquire-spot" timeline by:
///   1. Acquiring the per-channel persist lock first.
///   2. Mutating mailbox state UNDER that held lock to mark the
///      active turn as `our_msg_id` — i.e. the worst-case state the
///      old snapshot would have missed.
///   3. Calling `mailbox_snapshot` while still holding the lock and
///      asserting it observes the updated state.
///   4. Skipping the mapping insert (matching the production round-10
///      bail branch) and asserting `queued_placeholders` stays empty
///      and the on-disk persistence is also empty (no stale `📬` card
///      sidecar entry).
///
/// Pre round-10 (snapshot OUTSIDE the lock): step 3 would have used
/// the pre-step-2 snapshot value, decided "queued", and inserted a
/// stale mapping in step 4.
#[tokio::test(flavor = "current_thread")]
async fn race_loss_dispatch_state_recheck_under_persist_lock_skips_stale_insert() {
    use crate::services::provider::CancelToken;
    use std::sync::Arc;

    let shared = super::super::super::make_shared_data_for_tests();
    let channel_id = ChannelId::new(123_456_789);
    let owner = UserId::new(11);
    let our_msg_id = MessageId::new(7_777);
    let placeholder_msg_id = MessageId::new(8_888);

    // Acquire the per-channel persist lock FIRST (round-10
    // ordering). All `queued_placeholders` mutators serialize on this
    // mutex, so while we hold the guard nothing else can promote our
    // intervention into the map or out of it.
    let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
    let persist_guard = persist_lock.lock_owned().await;

    // Mutate mailbox state UNDER the held guard to simulate the
    // dispatch path advancing from "queued" to "active for our
    // user_msg_id" during the previous code's snapshot↔lock window.
    // In production this is the timeline:
    //   - active turn finishes
    //   - dispatch dequeues our intervention
    //   - dispatch starts a turn for our_msg_id
    //   - dispatch posts a fresh Active placeholder via the
    //     missing-mapping fallback
    // For the unit test we directly call `mailbox_try_start_turn` so
    // the snapshot's `active_user_message_id` equals `our_msg_id`,
    // which is the precise state the round-9 snapshot would have
    // missed but the round-10 snapshot must observe.
    let dispatch_token = Arc::new(CancelToken::new());
    let started = super::super::super::mailbox_try_start_turn(
        shared.as_ref(),
        channel_id,
        dispatch_token,
        owner,
        our_msg_id,
    )
    .await;
    assert!(
        started,
        "fresh mailbox must accept the dispatch-promoted turn"
    );

    // Snapshot UNDER the lock. Round-10: this is the round-9-residual
    // hazard's exact moment of truth — our path observes the
    // post-mutation state, not the pre-mutation snapshot.
    let snapshot = super::super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;
    let dispatch_already_running_for_our_msg = snapshot.active_user_message_id == Some(our_msg_id);
    assert!(
        dispatch_already_running_for_our_msg,
        "round-10: snapshot under the held persist lock must observe dispatch-already-running"
    );

    // Bail branch (matching the production code): do NOT call
    // `insert_queued_placeholder_locked`. The old code would have
    // inserted here because it snapshotted before the lock and
    // missed the dispatch promotion.
    if !dispatch_already_running_for_our_msg {
        shared.insert_queued_placeholder_locked(channel_id, our_msg_id, placeholder_msg_id);
    }
    drop(persist_guard);

    // Round-10 invariant: no stale mapping in memory.
    assert!(
        !shared
            .queued_placeholders
            .contains_key(&(channel_id, our_msg_id)),
        "round-10: no stale Queued mapping must be inserted when dispatch is already running for our_msg_id"
    );

    // And the ownership recheck (round-5 invariant) reports
    // not-owned, so the production `else if want_queued_card &&
    // !reused_existing_mapping` PATCH branch's first check would
    // skip the `ensure_queued` PATCH entirely — no stale `📬` card
    // gets rendered.
    assert!(
        !shared.queued_placeholder_still_owned(channel_id, our_msg_id, placeholder_msg_id),
        "queued_placeholder_still_owned must report not-owned so the PATCH branch skips the render"
    );
}

/// codex review round-11 P2 (#1332): the round-10 recheck only bailed
/// when `active_user_message_id == user_msg_id`, but other queue-exit
/// timelines also leave `user_msg_id` orphaned without making us the
/// active turn. Specifically:
///   - The intervention was cancelled / superseded between enqueue
///     and our lock acquire.
///   - The intervention is the non-head `source_message_id` of a
///     merged Intervention that has already been dequeued and its
///     merged-drain ran.
/// In those cases `active_user_message_id` may be `None` or a
/// different message, so the round-10 `active == user_msg_id` check
/// passes through and we would insert a `📬` mapping for a
/// `user_msg_id` that no future dispatch or queue-exit cleanup will
/// ever reference → stale card forever.
///
/// Round-11 fix: in addition to the round-10 active-equals-us check,
/// also verify `user_msg_id` is still in the queue (head
/// `intervention.message_id` OR any `source_message_ids` entry). If
/// neither holds, treat it as a race-loss and bail.
///
/// This test simulates the cancelled/superseded scenario where:
///   - `active_user_message_id == None` (no active turn — e.g. the
///     active turn finished and nothing else has started yet, OR the
///     channel never had an active turn after our enqueue was wiped).
///   - `intervention_queue` does NOT contain `our_msg_id` (queue
///     was drained / our entry was cancelled).
///
/// Pre round-11 (queue-membership check absent): the recheck would
/// pass through (active != us), the production code would insert a
/// `📬` mapping for our_msg_id, and the card would be left orphaned
/// forever.
#[tokio::test(flavor = "current_thread")]
async fn race_loss_recheck_bails_when_message_no_longer_queued() {
    let shared = super::super::super::make_shared_data_for_tests();
    let channel_id = ChannelId::new(424_242_424);
    let our_msg_id = MessageId::new(9_001);
    let placeholder_msg_id = MessageId::new(9_002);

    // Acquire the per-channel persist lock FIRST (round-10 / round-11
    // ordering). We do NOT enqueue our_msg_id and we do NOT start a
    // turn for our_msg_id, simulating the timeline where:
    //   - we enqueued, then released; queue-exit drain ran (cancel /
    //     supersede / merged-drain) and removed our_msg_id;
    //   - the active turn either finished or never picked us up;
    //   - we now take the persist lock to insert our `📬` mapping,
    //     observe `active_user_message_id == None` and a queue that
    //     no longer contains our_msg_id.
    let persist_lock = shared.queued_placeholders_persist_lock(channel_id);
    let persist_guard = persist_lock.lock_owned().await;

    // Snapshot UNDER the lock.
    let snapshot = super::super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;

    // Round-11 invariant: not the active turn.
    assert_eq!(
        snapshot.active_user_message_id, None,
        "test setup: no active turn so the round-10 condition active == us is FALSE",
    );
    // Round-11 invariant: queue does not contain our_msg_id.
    let still_queued = snapshot.intervention_queue.iter().any(|intervention| {
        intervention.message_id == our_msg_id
            || intervention.source_message_ids.contains(&our_msg_id)
    });
    assert!(
        !still_queued,
        "test setup: our_msg_id must NOT be in the queue (cancelled/superseded/merged-drained)",
    );

    // Compute the recheck condition exactly as the production code does.
    let dispatch_already_running_for_our_msg = snapshot.active_user_message_id == Some(our_msg_id);
    let should_bail = dispatch_already_running_for_our_msg || !still_queued;
    assert!(
        should_bail,
        "round-11: recheck must bail when message no longer queued, even if active != us",
    );

    // Production bail branch: do NOT call `insert_queued_placeholder_locked`.
    // Pre round-11 the broadened check did not exist, so the only
    // condition was `active == us`, which is FALSE here, and the code
    // would have inserted a stale `📬` mapping.
    if !should_bail {
        shared.insert_queued_placeholder_locked(channel_id, our_msg_id, placeholder_msg_id);
    }
    drop(persist_guard);

    // Round-11 invariant: no stale mapping in memory.
    assert!(
        !shared
            .queued_placeholders
            .contains_key(&(channel_id, our_msg_id)),
        "round-11: no stale Queued mapping must be inserted when message no longer queued",
    );

    // The ownership recheck reports not-owned, so the PATCH branch
    // would skip the `ensure_queued` render entirely — no stale `📬`
    // card surfaces.
    assert!(
        !shared.queued_placeholder_still_owned(channel_id, our_msg_id, placeholder_msg_id),
        "queued_placeholder_still_owned must report not-owned so the PATCH branch skips the render",
    );
}

#[test]
fn session_strategy_lifecycle_event_records_fresh_and_resumed_details() {
    let fresh = session_strategy_lifecycle_event(None, "no_cached_provider_session", None);
    assert_eq!(fresh.meta().kind, "session_fresh");
    assert!(!fresh.notify_user());
    let fresh_details = fresh.details_json();
    assert_eq!(fresh_details["reason"], "no_cached_provider_session");
    assert!(fresh_details["providerSessionId"].is_null());
    assert!(fresh_details["fingerprint"].is_null());

    let resumed = session_strategy_lifecycle_event(
        Some("provider-session-123"),
        "db_provider_session_restored",
        None,
    );
    assert_eq!(resumed.meta().kind, "session_resumed");
    assert!(!resumed.notify_user());
    let resumed_details = resumed.details_json();
    assert_eq!(resumed_details["reason"], "db_provider_session_restored");
    assert_eq!(resumed_details["providerSessionId"], "provider-session-123");
    assert_eq!(
        resumed_details["fingerprint"],
        crate::services::observability::turn_lifecycle::provider_session_fingerprint(
            "provider-session-123",
        )
    );
}
