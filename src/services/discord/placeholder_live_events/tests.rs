use super::super::formatting::{
    MonitorHandoffReason, MonitorHandoffStatus, build_monitor_handoff_placeholder_with_live_events,
    redact_sensitive_for_placeholder,
};
use super::common::{
    EVENT_BLOCK_MAX_CHARS, EVENT_LINE_MAX_CHARS, EVENT_RENDER_LIMIT, STATUS_PANEL_MAX_CHARS,
};
use super::*;
use serde_json::json;

#[test]
fn render_block_keeps_newest_events_under_limit() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(42);
    for idx in 0..25 {
        events.push_event(
            channel_id,
            RecentPlaceholderEvent::tool_use("Bash", &format!(r#"{{"command":"echo {idx}"}}"#))
                .unwrap(),
        );
    }

    let block = events.render_block(channel_id).unwrap();
    assert!(block.starts_with("```text\n"));
    assert!(block.ends_with("\n```"));
    assert!(block.chars().count() <= EVENT_BLOCK_MAX_CHARS);
    let live_lines = block
        .lines()
        .filter(|line| line.starts_with("[Bash]"))
        .collect::<Vec<_>>();
    assert_eq!(live_lines.len(), EVENT_RENDER_LIMIT);
    assert!(!block.contains("echo 19"));
    assert!(block.contains("echo 24"));
}

#[test]
fn events_from_json_redacts_and_normalizes_tool_use() {
    let events = events_from_json(&json!({
        "type": "assistant",
        "message": {
            "content": [{
                "type": "tool_use",
                "name": "Bash",
                "input": {"command": "curl -H 'Authorization: Bearer abc123' https://example.test?token=secret"}
            }]
        }
    }));

    assert_eq!(events.len(), 1);
    let line = events[0].render_line();
    assert!(line.starts_with("[Bash]"));
    assert!(line.contains("Bearer ***"));
    assert!(line.contains("token=***"));
    assert!(!line.contains("abc123"));
    assert!(!line.contains("secret"));
}

#[test]
fn redact_sensitive_for_placeholder_masks_required_patterns() {
    let redacted = redact_sensitive_for_placeholder(
        "sk-abcdefghijklmnopqrstuvwxyz \
         Authorization: Bearer live-token \
         password=hunter2 token=secret api_key=key1 api-key=key2 \
         alice@example.com",
    );

    assert!(redacted.contains("***"));
    assert!(redacted.contains("Bearer ***"));
    assert!(redacted.contains("password=***"));
    assert!(redacted.contains("token=***"));
    assert!(redacted.contains("api_key=***"));
    assert!(redacted.contains("api-key=***"));
    assert!(redacted.contains("***@***"));
    assert!(!redacted.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    assert!(!redacted.contains("live-token"));
    assert!(!redacted.contains("hunter2"));
    assert!(!redacted.contains("alice@example.com"));
    assert!(!redacted.contains("secret"));
    assert!(!redacted.contains("key1"));
    assert!(!redacted.contains("key2"));
}

#[test]
fn monitor_handoff_live_events_stays_under_description_limit_with_long_command() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(99);
    let long_command = format!(
        "printf '{}' && curl -H 'Authorization: Bearer secret-token' https://example.test?api_key=secret",
        "x".repeat(800)
    );
    for idx in 0..20 {
        events.push_event(
            channel_id,
            RecentPlaceholderEvent::tool_use(
                "Bash",
                &json!({"command": format!("{long_command}-{idx}")}).to_string(),
            )
            .unwrap(),
        );
    }

    let block = events.render_block(channel_id).unwrap();
    let live_lines = block
        .lines()
        .filter(|line| line.starts_with("[Bash]"))
        .collect::<Vec<_>>();
    assert!(!live_lines.is_empty());
    assert!(
        live_lines
            .iter()
            .all(|line| line.chars().count() <= EVENT_LINE_MAX_CHARS)
    );
    assert!(block.contains("..."));
    assert!(!block.contains("secret-token"));
    assert!(!block.contains("api_key=secret"));

    let rendered = build_monitor_handoff_placeholder_with_live_events(
        MonitorHandoffStatus::Active,
        MonitorHandoffReason::AsyncDispatch,
        1_700_000_000,
        Some(&"tool ".repeat(200)),
        Some(&long_command),
        Some(&"reason ".repeat(200)),
        Some(&"context ".repeat(200)),
        Some(&"request ".repeat(200)),
        Some(&"progress ".repeat(200)),
        Some(&block),
    );

    assert!(
        rendered.len() <= 4096,
        "monitor handoff placeholder exceeded embed description limit: {}",
        rendered.len()
    );
    assert!(rendered.contains("[Bash]"));
    assert!(rendered.contains("```text"));
}

#[test]
fn events_from_json_captures_task_notification() {
    let events = events_from_json(&json!({
        "type": "system",
        "subtype": "task_notification",
        "task_notification_kind": "background",
        "status": "completed",
        "summary": "CI green"
    }));

    assert_eq!(
        events,
        vec![RecentPlaceholderEvent {
            prefix: "[background]".to_string(),
            summary: "completed: CI green".to_string()
        }]
    );
}

#[test]
fn status_panel_renders_derived_tool_state_under_limit() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(77);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use("Bash", &json!({"command": "cargo test"}).to_string()),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("도구 실행 중"));
    assert!(rendered.contains("[Bash]"));
    assert!(rendered.chars().count() <= STATUS_PANEL_MAX_CHARS);
}

#[test]
fn status_panel_renders_session_resumed_line_from_lifecycle_details() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(177);
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        "session_resumed",
        &json!({
            "provider_session_id": "8f21abcd12345678",
            "tmux_reused": true
        }),
    ));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("Lifecycle resumed"));
    assert!(rendered.contains("provider session claude#8f21abcd…"));
    assert!(rendered.contains("tmux kept"));
}

#[test]
fn status_panel_renders_session_fresh_and_fallback_distinctly() {
    let events = PlaceholderLiveEvents::default();
    let fresh_channel_id = ChannelId::new(178);
    events.set_session_panel_lifecycle_event(
        fresh_channel_id,
        "session_fresh",
        &json!({
            "reason": "first_turn",
            "provider_session_id": "fresh-session-id",
            "tmux_reused": false
        }),
    );

    let fresh = events.render_status_panel(fresh_channel_id, &ProviderKind::Codex, 1_700_000_000);
    assert!(fresh.contains("Lifecycle fresh"));
    assert!(fresh.contains("provider session codex#fresh-se…"));
    assert!(fresh.contains("tmux new"));

    let fallback_channel_id = ChannelId::new(179);
    events.set_session_panel_lifecycle_event(
        fallback_channel_id,
        "session_resume_failed_with_recovery",
        &json!({
            "reason": "resume_failed",
            "providerSessionId": "fallback-session-id",
            "tmuxStatus": "kept"
        }),
    );

    let fallback =
        events.render_status_panel(fallback_channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(fallback.contains("Lifecycle fallback"));
    assert!(fallback.contains("provider session claude#fallback…"));
    assert!(fallback.contains("tmux kept"));
}

#[test]
fn status_panel_omits_session_line_when_lifecycle_details_are_absent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(180);
    assert!(!events.set_session_panel_lifecycle_event(channel_id, "session_resumed", &json!({}),));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(!rendered.contains("Lifecycle "));
}

#[test]
fn status_panel_omits_context_line_when_token_data_is_absent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(181);

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    assert!(!rendered.contains("Context   "));
}

#[test]
fn status_panel_renders_task_line_from_dispatch_metadata() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(185);
    assert!(events.set_task_panel_info(channel_id, "dsp_123", Some("42"), Some("implementation"),));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Codex, 1_700_000_000);

    assert!(rendered.contains("Task      dispatch #dsp\\_123 · card #42 · implementation"));
}

#[test]
fn status_panel_omits_task_line_without_dispatch_id() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(186);

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    assert!(!rendered.contains("Task      "));
}

#[test]
fn status_panel_renders_task_line_with_dispatch_fallback() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(187);
    assert!(events.set_task_panel_info(channel_id, "dsp_404", None, None));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    assert!(rendered.contains("Task      dispatch #dsp\\_404"));
    assert!(!rendered.contains("card #"));
}

#[test]
fn status_panel_renders_context_usage_severity_levels() {
    let events = PlaceholderLiveEvents::default();
    let normal_channel_id = ChannelId::new(182);
    assert!(events.set_context_panel_usage(normal_channel_id, 740, 0, 0, 1000, 90));
    let normal =
        events.render_status_panel(normal_channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(normal.contains("Context   📦 74% used · auto-compact 90%"));
    assert!(!normal.contains("임박"));
    assert!(!normal.contains("자동 압축 직전"));

    let approaching_channel_id = ChannelId::new(183);
    events.set_context_panel_usage(approaching_channel_id, 700, 40, 10, 1000, 90);
    let approaching =
        events.render_status_panel(approaching_channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(approaching.contains("Context   📦 75% used · auto-compact 90% (임박)"));

    let critical_channel_id = ChannelId::new(184);
    events.set_context_panel_usage(critical_channel_id, 700, 100, 50, 1000, 90);
    let critical =
        events.render_status_panel(critical_channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(critical.contains("Context   ⚠️ 85% used · auto-compact 90% — 자동 압축 직전"));
}

#[test]
fn status_panel_caps_context_usage_display_at_100_percent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(188);
    assert!(events.set_context_panel_usage(channel_id, 4000, 80, 10, 1000, 60));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    assert!(rendered.contains("Context   ⚠️ 100% used · auto-compact 60%"));
    assert!(!rendered.contains("409% used"));
}

#[test]
fn status_panel_renders_prompt_manifest_block() {
    fn layer(
        name: &str,
        enabled: bool,
        reason: Option<&str>,
    ) -> crate::db::prompt_manifests::PromptManifestLayer {
        crate::db::prompt_manifests::PromptManifestLayer {
            id: None,
            manifest_id: None,
            layer_name: name.to_string(),
            enabled,
            source: Some("test".to_string()),
            reason: reason.map(str::to_string),
            chars: 0,
            tokens_est: 0,
            content_sha256: "0".repeat(64),
            content_visibility: crate::db::prompt_manifests::PromptContentVisibility::AdkProvided,
            full_content: Some(String::new()),
            redacted_preview: None,
        }
    }

    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(185);
    let manifest = PromptManifest {
        id: None,
        created_at: None,
        turn_id: "turn-185".to_string(),
        channel_id: channel_id.get().to_string(),
        dispatch_id: None,
        profile: Some("full".to_string()),
        total_input_tokens_est: 21_400,
        layer_count: 5,
        layers: vec![
            layer("role_prompt", true, None),
            layer("dispatch_contract", true, None),
            layer("current_task", true, None),
            layer("recovery_context", false, Some("no_recovery")),
            layer(
                "memory_recall",
                false,
                Some("memory_backend=memento;mcp_unavailable"),
            ),
        ],
    };

    assert!(events.set_prompt_manifest(channel_id, &manifest));
    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("Prompt    Full profile · ~21.4k input tokens"));
    assert!(rendered.contains("- 활성 (3): role\\_prompt, dispatch\\_contract, current\\_task"));
    assert!(rendered.contains(
        "- 스킵 (2): recovery\\_context (no\\_recovery), memory\\_recall (memory\\_backend=memento;mcp\\_unavailable)"
    ));
}

#[test]
fn status_panel_omits_prompt_line_when_manifest_is_absent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(186);

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(!rendered.contains("Prompt    "));
}

#[test]
fn status_panel_tracks_todowrite_plan() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(78);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "Read issue", "status": "completed"},
                    {"content": "Implement panel", "status": "in_progress"}
                ]
            })
            .to_string(),
        ),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("Plan"));
    assert!(rendered.contains("- [x] Read issue"));
    assert!(rendered.contains("- [ ] Implement panel"));
}

#[test]
fn status_panel_tracks_one_level_subagents() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(79);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Inspect bridge"}).to_string(),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_task_notification("subagent", "running", "found turn bridge"),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result(Some("Task"), false),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("Subagents"));
    assert!(rendered.contains("explorer Inspect bridge"));
    assert!(rendered.contains("found turn bridge"));
    assert!(rendered.contains("✓"));
}

#[test]
fn status_panel_hides_plan_and_subagents_for_codex() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(80);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "TodoWrite",
            &json!({"todos": [{"content": "Hidden for Codex", "status": "pending"}]}).to_string(),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"description": "Hidden subagent"}).to_string(),
        ),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Codex, 1_700_000_000);
    assert!(!rendered.contains("Plan"));
    assert!(!rendered.contains("Subagents"));
    assert!(!rendered.contains("Hidden for Codex"));
    assert!(!rendered.contains("Hidden subagent"));
}

#[test]
fn status_events_from_json_keeps_tool_result_visibility() {
    let events = status_events_from_json(&json!({
        "type": "user",
        "message": {
            "content": [{
                "type": "tool_result",
                "is_error": true,
                "content": "failed"
            }]
        }
    }));

    assert_eq!(events, vec![StatusEvent::ToolEnd { success: false }]);
}

#[test]
fn status_tool_result_closes_subagent_only_for_task_tools() {
    assert_eq!(
        status_events_from_tool_result(Some("Read"), false),
        vec![StatusEvent::ToolEnd { success: true }]
    );
    assert_eq!(
        status_events_from_tool_result(Some("Task"), true),
        vec![
            StatusEvent::ToolEnd { success: false },
            StatusEvent::SubagentEnd { success: false }
        ]
    );
}
