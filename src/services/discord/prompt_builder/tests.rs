use super::layer_rendering::{
    CONTEXT_COMPRESSION_SECTION_ORDER, STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE,
    agent_performance_prompt_section_with_loader, reset_agent_performance_cache_for_tests,
};
use super::manifest::ROLE_PROMPT_LAYER_NAME;
use super::*;
use crate::db::prompt_manifests::PromptContentVisibility;
use crate::services::discord::settings::MemoryBackendKind;
use std::sync::{Mutex, OnceLock};

/// Helper: call build_system_prompt with minimal/default arguments (Full profile),
/// while requiring each test to choose its own memento availability.
fn call_build(
    discord_context: &str,
    current_path: &str,
    channel_id: u64,
    token: &str,
    memento_mcp_available: bool,
) -> String {
    build_system_prompt(
        discord_context,
        &[],
        current_path,
        ChannelId::new(channel_id),
        token,
        None,  // role_binding
        false, // queued_turn
        DispatchProfile::Full,
        None, // dispatch_type
        None, // current_task
        None, // shared_knowledge
        None, // longterm_catalog
        None, // memory_settings
        memento_mcp_available,
    )
}

#[test]
fn test_build_system_prompt_includes_discord_context() {
    let output = call_build(
        "Channel: #general (guild: TestServer)",
        "/tmp/work",
        123456789,
        "fake-token",
        false,
    );
    assert!(
        output.contains("Channel: #general (guild: TestServer)"),
        "System prompt should contain the discord_context string"
    );
}

#[test]
fn test_build_system_prompt_lists_channel_participants_without_inline_context_user() {
    let participants = [UserRecord::new(UserId::new(77), "Alice")];
    let output = build_system_prompt(
        "Discord context: channel #general (ID: 42)",
        &participants,
        "/tmp/work",
        ChannelId::new(42),
        "fake-token",
        None,
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        None,
        None,
        false,
    );

    assert!(output.contains("Channel participants:\n- Alice (ID: 77)"));
    assert!(output.contains("[User: NAME (ID: N)]"));
    let discord_context_line = output
        .lines()
        .find(|line| line.starts_with("Discord context:"))
        .expect("discord context line");
    assert!(!discord_context_line.contains("user: Alice"));
    assert!(!discord_context_line.contains("ID: 77"));
}

#[test]
fn test_build_system_prompt_marks_dm_single_participant() {
    let participants = [UserRecord::new(UserId::new(77), "Alice")];
    let output = build_system_prompt(
        "Discord context: DM",
        &participants,
        "/tmp/work",
        ChannelId::new(42),
        "fake-token",
        None,
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        None,
        None,
        false,
    );

    assert!(output.contains("Channel participants:\n- Alice (ID: 77) [DM requester]"));
}

#[test]
fn test_build_system_prompt_includes_cwd() {
    let output = call_build("ctx", "/home/user/projects", 1, "tok", false);
    assert!(
        output.contains("Current working directory: /home/user/projects"),
        "System prompt should contain the current working directory"
    );
}

#[test]
fn test_build_system_prompt_includes_file_send_command() {
    let output = call_build("ctx", "/tmp", 1, "tok", false);
    assert!(
        output.contains("agentdesk discord-sendfile"),
        "System prompt should contain the agentdesk discord-sendfile command"
    );
}

#[test]
fn test_build_system_prompt_disables_interactive_tools() {
    let output = call_build("ctx", "/tmp", 1, "tok", false);
    assert!(
        output.contains("does not support interactive prompts"),
        "System prompt should warn that interactive tools are disabled"
    );
    assert!(
        output.contains("Do NOT call AskUserQuestion"),
        "System prompt should instruct not to use interactive tools"
    );
}

#[test]
fn test_build_system_prompt_includes_context_compression_guidance() {
    let output = call_build("ctx", "/tmp", 1, "tok", false);
    assert!(output.contains("[Context Compression]"));
    assert!(output.contains(CONTEXT_COMPRESSION_SECTION_ORDER));
    assert!(output.contains(STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE));
}

#[test]
fn test_build_system_prompt_includes_tool_output_efficiency_guidance() {
    let output = call_build("ctx", "/tmp", 1, "tok", false);
    assert!(output.contains("[Tool Output Efficiency]"));
    assert!(output.contains("Large tool results persist in context"));
    assert!(output.contains("If output would exceed 10 lines"));
    assert!(output.contains("Use LIMIT clauses for SQL"));
    assert!(output.contains("Use offset/limit to read specific sections"));
    assert!(output.contains("do not read entire files"));
    assert!(output.contains("Set head_limit"));
}

#[test]
fn test_build_system_prompt_includes_api_friction_guidance() {
    let output = call_build("ctx", "/tmp", 1, "tok", false);
    assert!(output.contains("[ADK API Usage]"));
    assert!(output.contains("GET /api/docs/{category}"));
    assert!(output.contains("API_FRICTION:"));
    assert!(output.contains("topic=api-friction"));
}

#[test]
fn test_dispatch_profile_from_dispatch_type() {
    assert_eq!(
        DispatchProfile::from_dispatch_type(None),
        DispatchProfile::Full
    );
    assert_eq!(
        DispatchProfile::from_dispatch_type(Some("implementation")),
        DispatchProfile::Full
    );
    assert_eq!(
        DispatchProfile::from_dispatch_type(Some("review")),
        DispatchProfile::ReviewLite
    );
    assert_eq!(
        DispatchProfile::from_dispatch_type(Some("review-decision")),
        DispatchProfile::ReviewLite
    );
    assert_eq!(
        DispatchProfile::from_dispatch_type(Some("e2e-test")),
        DispatchProfile::Full
    );
    assert_eq!(
        DispatchProfile::from_dispatch_type(Some("consultation")),
        DispatchProfile::Full
    );
    assert_eq!(
        DispatchProfile::from_dispatch_type(Some("rework")),
        DispatchProfile::Full
    );
}

#[test]
fn test_dispatch_profile_for_turn_respects_channel_lite_except_reviews() {
    assert_eq!(
        DispatchProfile::for_turn(None, Some(DispatchProfile::Lite)),
        DispatchProfile::Lite
    );
    assert_eq!(
        DispatchProfile::for_turn(Some("implementation"), Some(DispatchProfile::Lite)),
        DispatchProfile::Lite
    );
    assert_eq!(
        DispatchProfile::for_turn(Some("review"), Some(DispatchProfile::Lite)),
        DispatchProfile::ReviewLite
    );
    assert_eq!(
        DispatchProfile::for_turn(None, Some(DispatchProfile::Full)),
        DispatchProfile::Full
    );
}

#[test]
fn test_empty_skills_notice_omits_skills_for_full_profile() {
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        None,
        None,
        false,
    );

    assert!(!prompt.contains("Available skills"));
    assert!(!prompt.contains("descriptions only"));
    assert!(!prompt.contains("`SKILL.md`"));
}

#[test]
fn test_review_lite_omits_context_compression_guidance() {
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::ReviewLite,
        Some("review"),
        None,
        None,
        None,
        None,
        false,
    );

    assert!(!prompt.contains("[Context Compression]"));
    assert!(!prompt.contains(CONTEXT_COMPRESSION_SECTION_ORDER));
    assert!(!prompt.contains(STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE));
}

#[test]
fn test_review_lite_includes_tool_output_efficiency_guidance() {
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::ReviewLite,
        Some("review"),
        None,
        None,
        None,
        None,
        false,
    );

    assert!(prompt.contains("[Tool Output Efficiency]"));
    assert!(prompt.contains("Prefer targeted queries over exhaustive dumps"));
}

#[test]
fn test_role_prompt_layer_records_adk_provided_full_content_and_sha() {
    use super::super::settings::RoleBinding;
    use sha2::{Digest, Sha256};

    let temp = tempfile::tempdir().expect("tempdir");
    let prompt_path = temp.path().join("project-agentdesk.prompt.md");
    let role_prompt = "# AgentDesk\n\nFollow project rules.\n";
    std::fs::write(&prompt_path, role_prompt).expect("write role prompt");
    let binding = RoleBinding {
        role_id: "project-agentdesk".to_string(),
        prompt_file: prompt_path.display().to_string(),
        provider: None,
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: false,
        quality_feedback_injection_enabled: true,
        memory: Default::default(),
    };

    let built = build_system_prompt_with_manifest(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        None,
        None,
        false,
        None,
        None,
        Some("turn-1"),
    );

    assert!(built.system_prompt.contains(role_prompt));
    let manifest = built.manifest.expect("prompt manifest");
    assert_eq!(manifest.turn_id, "turn-1");
    assert_eq!(manifest.channel_id, "1");
    assert_eq!(manifest.profile.as_deref(), Some("full"));
    assert_eq!(manifest.layer_count, 3);
    let layer = manifest
        .layers
        .iter()
        .find(|layer| layer.layer_name == ROLE_PROMPT_LAYER_NAME)
        .expect("role prompt layer");
    assert!(layer.enabled);
    assert_eq!(
        layer.source.as_deref(),
        Some("agents/project-agentdesk.prompt.md")
    );
    assert_eq!(layer.reason.as_deref(), Some("agent_id=project-agentdesk"));
    assert_eq!(
        layer.content_visibility,
        PromptContentVisibility::AdkProvided
    );
    assert_eq!(layer.full_content.as_deref(), Some(role_prompt));
    assert_eq!(layer.redacted_preview, None);
    assert_eq!(
        layer.content_sha256,
        hex::encode(Sha256::digest(role_prompt.as_bytes()))
    );
    assert_eq!(layer.chars, role_prompt.chars().count() as i64);
}

#[test]
fn test_lite_profile_uses_abbreviated_rules_and_omits_role_prompt() {
    use super::super::settings::RoleBinding;

    let binding = RoleBinding {
        role_id: "test-agent".to_string(),
        prompt_file: "/nonexistent".to_string(),
        provider: None,
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: true,
        quality_feedback_injection_enabled: true,
        memory: Default::default(),
    };
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::Lite,
        None,
        None,
        Some("[Shared Agent Knowledge]\nlarge shared block"),
        Some("- facts.md"),
        Some(&ResolvedMemorySettings {
            backend: MemoryBackendKind::Memento,
            ..ResolvedMemorySettings::default()
        }),
        true,
    );

    assert!(prompt.contains("[Lite Channel Rules]"));
    assert!(!prompt.contains("[Shared Agent Rules]"));
    assert!(!prompt.contains("[Channel Role Binding]"));
    assert!(!prompt.contains("[Shared Agent Knowledge]"));
    assert!(!prompt.contains("[Long-term Memory]"));
    assert!(!prompt.contains("[Proactive Memory Guidance]"));
    assert!(!prompt.contains("[ADK API Usage]"));
}

#[test]
fn test_review_decision_gets_decision_rules() {
    use super::super::settings::RoleBinding;
    let binding = RoleBinding {
        role_id: "test-agent".to_string(),
        prompt_file: "/nonexistent".to_string(),
        provider: None,
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: true,
        quality_feedback_injection_enabled: true,
        memory: Default::default(),
    };
    let review_prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::ReviewLite,
        Some("review"),
        None,
        None,
        None,
        None,
        false,
    );
    let decision_prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::ReviewLite,
        Some("review-decision"),
        None,
        None,
        None,
        None,
        false,
    );
    // review should NOT contain decision API
    assert!(!review_prompt.contains("/api/reviews/decision"));
    assert!(review_prompt.contains("[Review Rules]"));
    // review-decision should contain decision API and options
    assert!(decision_prompt.contains("/api/reviews/decision"));
    assert!(decision_prompt.contains("accept/dispute/dismiss"));
    assert!(decision_prompt.contains("[Review Decision Rules]"));
}

#[test]
fn test_full_prompt_omits_peer_agent_directory_when_disabled() {
    use super::super::settings::RoleBinding;

    let binding = RoleBinding {
        role_id: "spark".to_string(),
        prompt_file: "/nonexistent".to_string(),
        provider: None,
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: false,
        quality_feedback_injection_enabled: true,
        memory: Default::default(),
    };

    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1488022491992424448),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        None,
        None,
        false,
    );

    assert!(!prompt.contains("[Peer Agent Directory]"));
}

#[test]
fn test_full_prompt_renders_supplied_longterm_catalog() {
    use super::super::settings::RoleBinding;

    let binding = RoleBinding {
        role_id: "spark".to_string(),
        prompt_file: "/nonexistent".to_string(),
        provider: None,
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: false,
        quality_feedback_injection_enabled: true,
        memory: Default::default(),
    };

    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        Some("- facts.md: deployment notes"),
        None,
        false,
    );

    assert!(prompt.contains("[Long-term Memory]"));
    assert!(prompt.contains("facts.md"));
}

#[test]
fn test_full_prompt_injects_memento_memory_guidance() {
    use super::super::settings::RoleBinding;

    let binding = RoleBinding {
        role_id: "project-agentdesk".to_string(),
        prompt_file: "/nonexistent".to_string(),
        provider: None,
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: false,
        quality_feedback_injection_enabled: true,
        memory: Default::default(),
    };
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/Users/test/.adk/release/workspaces/agentdesk",
        ChannelId::new(1),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        None,
        Some(&ResolvedMemorySettings {
            backend: MemoryBackendKind::Memento,
            ..ResolvedMemorySettings::default()
        }),
        true,
    );

    assert!(prompt.contains("[Proactive Memory Guidance]"));
    assert!(prompt.contains("`recall` MCP tool"));
    assert!(prompt.contains("`remember` MCP tool"));
    assert!(prompt.contains("full memory policy: `docs/memory-scope.md`"));
    assert!(prompt.contains("project=`workspace=agentdesk, agentId=default`"));
    assert!(prompt.contains(
        "agent-private=`workspace=agentdesk-project-agentdesk, agentId=project-agentdesk`"
    ));
    assert!(!prompt.contains("tool_feedback("));
}

#[test]
fn test_full_prompt_omits_memento_memory_guidance_without_mcp() {
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/Users/test/.adk/release/workspaces/agentdesk",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::Full,
        None,
        None,
        None,
        None,
        Some(&ResolvedMemorySettings {
            backend: MemoryBackendKind::Memento,
            ..ResolvedMemorySettings::default()
        }),
        false,
    );

    assert!(!prompt.contains("[Proactive Memory Guidance]"));
    assert!(!prompt.contains("`recall` MCP tool"));
    assert!(!prompt.contains("`remember` MCP tool"));
}

#[test]
fn test_review_lite_omits_memory_guidance() {
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::ReviewLite,
        Some("review"),
        None,
        None,
        None,
        Some(&ResolvedMemorySettings {
            backend: MemoryBackendKind::File,
            ..ResolvedMemorySettings::default()
        }),
        false,
    );

    assert!(!prompt.contains("[Proactive Memory Guidance]"));
    assert!(!prompt.contains("`memory-read`"));
    assert!(!prompt.contains("`memory-write`"));
}

#[test]
fn test_build_system_prompt_appends_current_task_after_queued_turn_rules() {
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-570"),
        card_id: Some("card-570"),
        dispatch_title: Some("[Rework] fix: prompt context"),
        dispatch_context: None,
        card_title: Some("fix: prompt context"),
        github_issue_url: Some("https://github.com/itismyfield/AgentDesk/issues/570"),
    };
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        None,
        true,
        DispatchProfile::Full,
        Some("implementation"),
        Some(&current_task),
        None,
        None,
        None,
        false,
    );

    let queued_index = prompt.find("[Queued Turn Rules]").unwrap();
    let task_index = prompt.find("[Current Task]").unwrap();
    assert!(task_index > queued_index);
    assert!(prompt.contains("Dispatch ID: dispatch-570"));
    assert!(prompt.contains("Card ID: card-570"));
    assert!(prompt.contains("Dispatch Brief:\n[Rework] fix: prompt context"));
    assert!(prompt.contains("GitHub URL: https://github.com/itismyfield/AgentDesk/issues/570"));
    assert!(prompt.contains("Title: fix: prompt context"));
    assert!(prompt.contains("`OUTCOME: noop`"));
    assert!(!prompt.contains("Issue Body:"));
    assert!(!prompt.contains("DoD:"));
}

#[test]
fn test_build_system_prompt_renders_dispatch_context_and_completion_contract() {
    let dispatch_context = serde_json::json!({
        "repo": "owner/repo",
        "issue_number": 671,
        "pr_number": 812,
        "review_mode": "noop_verification",
        "branch": "wt/671-dispatch",
        "reviewed_commit": "abc12345deadbeef",
        "merge_base": "1122334455667788",
        "noop_reason": "feature already exists",
        "review_quality_checklist": ["edge case", "error handling"],
        "review_verdict_guidance": "quality issue가 보이면 improve",
        "verdict_endpoint": "POST /api/reviews/verdict",
        "ci_recovery": {
            "job_name": "dashboard-build",
            "reason": "Code job failed: dashboard-build",
            "run_url": "https://github.com/example/actions/runs/1"
        }
    });
    let dispatch_context_raw = dispatch_context.to_string();
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-review-671"),
        card_id: Some("card-671"),
        dispatch_title: Some("[Review R2] card-671"),
        dispatch_context: Some(&dispatch_context_raw),
        card_title: Some("fix: dispatch message"),
        github_issue_url: None,
    };
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::ReviewLite,
        Some("review"),
        Some(&current_task),
        None,
        None,
        None,
        false,
    );

    assert!(prompt.contains("Review Repo: owner/repo"));
    assert!(prompt.contains("Review Issue: #671"));
    assert!(prompt.contains("Review PR: #812"));
    assert!(prompt.contains("Review Mode: noop_verification"));
    assert!(prompt.contains("Review Branch: wt/671-dispatch"));
    assert!(prompt.contains("Reviewed Commit: abc12345deadbeef"));
    assert!(prompt.contains("Verdict Endpoint: POST /api/reviews/verdict"));
    assert!(prompt.contains("CI Recovery Job: dashboard-build"));
    assert!(prompt.contains("`POST /api/reviews/verdict` (`dispatch_id=dispatch-review-671`)"));
    assert!(prompt.contains("Review Quality Checklist"));
}

#[test]
fn test_review_decision_identifiers_render_in_current_task_but_not_rules_section() {
    use super::super::settings::RoleBinding;

    let dispatch_context = serde_json::json!({
        "repo": "owner/repo",
        "issue_number": 692,
        "pr_number": 366,
        "reviewed_commit": "feedfacecafebeef",
        "decision_endpoint": "POST /api/reviews/decision",
        "verdict": "rework"
    });
    let dispatch_context_raw = dispatch_context.to_string();
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-decision-692"),
        card_id: Some("card-692"),
        dispatch_title: Some("[리뷰 검토] card-692"),
        dispatch_context: Some(&dispatch_context_raw),
        card_title: Some("refactor: self-contained review decision"),
        github_issue_url: Some("https://github.com/itismyfield/AgentDesk/issues/692"),
    };
    let binding = RoleBinding {
        role_id: "test-agent".to_string(),
        prompt_file: "/nonexistent".to_string(),
        provider: None,
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: true,
        quality_feedback_injection_enabled: true,
        memory: Default::default(),
    };

    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        Some(&binding),
        false,
        DispatchProfile::ReviewLite,
        Some("review-decision"),
        Some(&current_task),
        None,
        None,
        None,
        false,
    );

    let rules_start = prompt.find("[Review Decision Rules]").unwrap();
    let task_start = prompt.find("[Current Task]").unwrap();
    let rules_section = &prompt[rules_start..task_start];

    assert!(prompt.contains("Review Repo: owner/repo"));
    assert!(prompt.contains("Review Issue: #692"));
    assert!(prompt.contains("Review PR: #366"));
    assert!(prompt.contains("Reviewed Commit: feedfacecafebeef"));
    assert!(prompt.contains("Decision Endpoint: POST /api/reviews/decision"));
    assert!(rules_section.contains("POST /api/reviews/decision {card_id, decision, comment}"));
    assert!(!rules_section.contains("owner/repo"));
    assert!(!rules_section.contains("#366"));
    assert!(!rules_section.contains("feedfacecafebeef"));
}

#[test]
fn test_build_system_prompt_keeps_dispatch_contract_when_context_is_otherwise_empty() {
    let current_task = CurrentTaskContext::default();
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::Full,
        Some("implementation"),
        Some(&current_task),
        None,
        None,
        None,
        false,
    );

    assert!(prompt.contains("[Current Task]"));
    assert!(prompt.contains("[Dispatch Contract]"));
    assert!(prompt.contains("`OUTCOME: noop`"));
    assert!(prompt.contains("`git push origin HEAD:main`"));
    assert!(prompt.contains("PR fallback"));
    assert!(!prompt.contains("Dispatch ID:"));
    assert!(!prompt.contains("GitHub URL:"));
}

#[test]
fn test_build_system_prompt_uses_direct_first_completion_contract_by_default() {
    let dispatch_context = serde_json::json!({
        "merge_strategy_mode": "direct-first"
    });
    let dispatch_context_raw = dispatch_context.to_string();
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-direct-1"),
        dispatch_context: Some(&dispatch_context_raw),
        ..CurrentTaskContext::default()
    };
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::Full,
        Some("implementation"),
        Some(&current_task),
        None,
        None,
        None,
        false,
    );

    assert!(prompt.contains("`merge_strategy_mode=direct-first`"));
    assert!(prompt.contains("`git push origin HEAD:main`"));
    assert!(prompt.contains("PR fallback"));
    assert!(prompt.contains("PATCH /api/dispatches/dispatch-direct-1"));
    assert!(prompt.contains("\"completed_commit\":\"<HEAD SHA>\""));
    assert!(prompt.contains("`▶ Ready for input (type message + Enter)` 는 완료 마커가 아니다."));
}

#[test]
fn test_build_system_prompt_uses_pr_always_completion_contract_when_requested() {
    let dispatch_context = serde_json::json!({
        "merge_strategy_mode": "pr-always"
    });
    let dispatch_context_raw = dispatch_context.to_string();
    let current_task = CurrentTaskContext {
        dispatch_context: Some(&dispatch_context_raw),
        ..CurrentTaskContext::default()
    };
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::Full,
        Some("implementation"),
        Some(&current_task),
        None,
        None,
        None,
        false,
    );

    assert!(prompt.contains("`merge_strategy_mode=pr-always`"));
    assert!(prompt.contains("별도 브랜치에서 작업"));
    assert!(prompt.contains("PR 을 연다"));
    assert!(prompt.contains("auto-merge enable"));
    assert!(prompt.contains("`▶ Ready for input (type message + Enter)` 는 완료 마커가 아니다."));
}

#[test]
fn test_build_system_prompt_uses_default_dispatch_contract_for_unknown_dispatch_type() {
    let current_task = CurrentTaskContext {
        dispatch_id: Some("dispatch-generic-1"),
        ..CurrentTaskContext::default()
    };
    let prompt = build_system_prompt(
        "ctx",
        &[],
        "/tmp",
        ChannelId::new(1),
        "tok",
        None,
        false,
        DispatchProfile::Full,
        None,
        Some(&current_task),
        None,
        None,
        None,
        false,
    );

    assert!(prompt.contains("[Dispatch Contract]"));
    assert!(prompt.contains("PATCH /api/dispatches/dispatch-generic-1"));
    assert!(prompt.contains("별도 review verdict/review-decision 규칙이 없으면"));
}

// NOTE: The _shared.prompt.md content assertion test was removed when
// per-agent prompts moved out of the repo (operator-private content, now
// canonical in the operator's Obsidian vault — see docs/source-of-truth.md).
// Content-level validation now lives with the prompt author's editor workflow.

// ─────────────────────────────────────────────────────────────────────
// #1103 — Self-feedback prompt block tests
//
// These tests cover the *cache* and *channel A/B toggle* layers. The
// formatter and rework category classifier are tested directly in
// `internal_api::self_feedback_tests` against `AgentQualitySnapshot` so
// they don't need a Postgres pool.
// ─────────────────────────────────────────────────────────────────────

use super::super::settings::RoleBinding;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Serialise the cache-aware tests — they share a process-wide static
/// cache, so cargo's parallel test runner would otherwise interleave
/// `reset_agent_performance_cache_for_tests` calls with concurrent
/// `lookup_cached_agent_performance_section` reads from sibling tests.
fn cache_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn make_role_binding(role_id: &str, quality_feedback_enabled: bool) -> RoleBinding {
    RoleBinding {
        role_id: role_id.to_string(),
        prompt_file: "/nonexistent".to_string(),
        provider: None,
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: false,
        quality_feedback_injection_enabled: quality_feedback_enabled,
        memory: Default::default(),
    }
}

#[test]
fn self_feedback_section_is_cached_within_same_hour_bucket() {
    let _guard = cache_test_lock();
    reset_agent_performance_cache_for_tests();
    let binding = make_role_binding("role-cache-stable", true);
    let calls = AtomicUsize::new(0);
    let bucket = 42_i64;

    let first = agent_performance_prompt_section_with_loader(
        Some(&binding),
        DispatchProfile::Full,
        bucket,
        |role_id| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(format!(
                "[Agent Performance — Last 7 Days]\nrole={role_id}"
            )))
        },
    );
    let second = agent_performance_prompt_section_with_loader(
        Some(&binding),
        DispatchProfile::Full,
        bucket,
        |_| {
            panic!("loader must not run for a same-bucket cache hit");
        },
    );

    assert_eq!(first, second);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "loader hit exactly once");
    assert!(first.unwrap().contains("role=role-cache-stable"));
}

#[test]
fn self_feedback_section_recomputes_after_bucket_rollover() {
    let _guard = cache_test_lock();
    reset_agent_performance_cache_for_tests();
    let binding = make_role_binding("role-bucket-roll", true);

    let prev = agent_performance_prompt_section_with_loader(
        Some(&binding),
        DispatchProfile::Full,
        100,
        |_| Ok(Some("v1".to_string())),
    );
    let next = agent_performance_prompt_section_with_loader(
        Some(&binding),
        DispatchProfile::Full,
        101,
        |_| Ok(Some("v2".to_string())),
    );

    assert_eq!(prev, Some("v1".to_string()));
    assert_eq!(next, Some("v2".to_string()));
}

#[test]
fn self_feedback_section_skipped_when_channel_toggle_off() {
    let _guard = cache_test_lock();
    reset_agent_performance_cache_for_tests();
    let binding = make_role_binding("role-toggle-off", false);
    let calls = AtomicUsize::new(0);

    let result = agent_performance_prompt_section_with_loader(
        Some(&binding),
        DispatchProfile::Full,
        7,
        |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some("should-not-render".to_string()))
        },
    );

    assert!(result.is_none());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "loader must not run when toggle is off"
    );
}

#[test]
fn self_feedback_section_skipped_for_review_lite() {
    let _guard = cache_test_lock();
    reset_agent_performance_cache_for_tests();
    let binding = make_role_binding("role-review-lite", true);

    let result = agent_performance_prompt_section_with_loader(
        Some(&binding),
        DispatchProfile::ReviewLite,
        7,
        |_| Ok(Some("never".to_string())),
    );

    assert!(result.is_none());
}

#[test]
fn self_feedback_section_caches_negative_lookup() {
    // Anthropic cache hit also relies on stability when the loader returns
    // None (e.g. fresh agent with no rollup row yet) — the cached `None`
    // must be served on subsequent calls so the prompt prefix stays
    // identical until the bucket rolls.
    let _guard = cache_test_lock();
    reset_agent_performance_cache_for_tests();
    let binding = make_role_binding("role-empty", true);
    let calls = AtomicUsize::new(0);

    let first = agent_performance_prompt_section_with_loader(
        Some(&binding),
        DispatchProfile::Full,
        9,
        |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        },
    );
    let second = agent_performance_prompt_section_with_loader(
        Some(&binding),
        DispatchProfile::Full,
        9,
        |_| panic!("loader must not run on cached negative hit"),
    );

    assert!(first.is_none());
    assert!(second.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn self_feedback_section_skips_when_role_binding_absent() {
    let _guard = cache_test_lock();
    reset_agent_performance_cache_for_tests();
    let result =
        agent_performance_prompt_section_with_loader(None, DispatchProfile::Full, 1, |_| {
            panic!("loader must not run without a binding")
        });
    assert!(result.is_none());
}
