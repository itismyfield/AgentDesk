use super::super::formatting::{
    MonitorHandoffReason, MonitorHandoffStatus, build_monitor_handoff_placeholder_with_live_events,
    redact_sensitive_for_placeholder,
};
use super::common::{
    EVENT_BLOCK_MAX_CHARS, EVENT_LINE_MAX_CHARS, EVENT_RENDER_LIMIT, STATUS_PANEL_MAX_CHARS,
    STATUS_PANEL_TASK_LIMIT,
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
fn events_from_json_filters_internal_cancellation_tool_error() {
    let events = events_from_json(&json!({
        "type": "user",
        "message": {
            "content": [{
                "type": "tool_result",
                "is_error": true,
                "content": "Cancelled: parallel tool call Bash(echo hi)"
            }]
        }
    }));

    assert!(
        events.is_empty(),
        "internal cancellation diagnostics must not reach the Recent mirror"
    );
}

#[test]
fn events_from_json_keeps_genuine_tool_error() {
    let events = events_from_json(&json!({
        "type": "user",
        "message": {
            "content": [{
                "type": "tool_result",
                "is_error": true,
                "content": "ENOENT: no such file or directory"
            }]
        }
    }));

    assert_eq!(events.len(), 1);
    let line = events[0].render_line();
    assert!(line.starts_with("[tool error]"));
    assert!(line.contains("ENOENT"));
}

#[test]
fn result_event_filters_internal_cancellation_tool_error() {
    let events = events_from_json(&json!({
        "type": "result",
        "is_error": true,
        "result": "Cancelled: parallel tool call Bash(echo hi)"
    }));

    assert!(events.is_empty());
}

#[test]
fn events_from_json_keeps_genuine_cancelled_prefixed_tool_error() {
    // A real tool/dispatch failure whose summary merely begins with
    // "Cancelled:" (but is not the harness parallel-tool-call diagnostic) must
    // still surface so the Recent mirror preserves real failure visibility.
    let events = events_from_json(&json!({
        "type": "user",
        "message": {
            "content": [{
                "type": "tool_result",
                "is_error": true,
                "content": "Cancelled: terminal card cleanup"
            }]
        }
    }));

    assert_eq!(events.len(), 1);
    let line = events[0].render_line();
    assert!(line.starts_with("[tool error]"));
    assert!(line.contains("terminal card cleanup"));
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

fn status_for(events: &PlaceholderLiveEvents, channel_id: ChannelId) -> DerivedStatus {
    events
        .status_by_channel
        .get(&channel_id)
        .expect("status panel state")
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .status
        .clone()
}

#[test]
fn status_panel_turn_completed_renders_foreground_completion() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(171);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use("Bash", &json!({"command": "cargo test"}).to_string()),
    );
    events.push_status_event(channel_id, StatusEvent::TurnCompleted { background: false });

    assert_eq!(
        status_for(&events, channel_id),
        DerivedStatus::Completed {
            kind: CompletedKind::Foreground
        }
    );
    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.starts_with("✅ **응답 완료** — claude"));
    assert!(!rendered.contains("🟢 진행 중"));
}

#[test]
fn status_panel_turn_completed_drops_recent_live_block() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(174);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Bash",
            &json!({"command": "printf E2E_TOOL_OK"}).to_string(),
        ),
    );
    events.push_event(
        channel_id,
        RecentPlaceholderEvent::tool_use(
            "Bash",
            &json!({"command": "printf E2E_TOOL_OK"}).to_string(),
        )
        .unwrap(),
    );

    let active = events.render_status_panel_with_heartbeat(
        channel_id,
        &ProviderKind::Claude,
        1_700_000_000,
        1_700_000_005,
    );
    assert!(active.contains("🖥️ Recent"));
    assert!(active.contains("[Bash]"));
    assert!(!active.contains("계속 처리 중"));

    events.push_status_event(channel_id, StatusEvent::TurnCompleted { background: false });

    let completed = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(completed.starts_with("✅ **응답 완료** — claude"));
    assert!(!completed.contains("🖥️ Recent"));
    assert!(!completed.contains("[Bash]"));
    assert!(!completed.contains("계속 처리 중"));
}

#[test]
fn status_panel_codex_active_omits_processing_tail_after_recent_block() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(175);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Bash",
            &json!({"command": "cargo test --package agentdesk"}).to_string(),
        ),
    );
    events.push_event(
        channel_id,
        RecentPlaceholderEvent::tool_use(
            "Bash",
            &json!({"command": "cargo test --package agentdesk"}).to_string(),
        )
        .unwrap(),
    );

    let rendered = events.render_status_panel_with_heartbeat(
        channel_id,
        &ProviderKind::Codex,
        1_700_000_000,
        1_700_000_005,
    );

    assert!(rendered.contains("🔧 도구 실행 중"));
    assert!(rendered.contains("🖥️ Recent"));
    assert!(rendered.contains("[Bash]"));
    assert!(!rendered.contains("계속 처리 중"));
}

#[test]
fn status_panel_truncates_long_body_without_processing_tail() {
    let sections = vec!["x".repeat(STATUS_PANEL_MAX_CHARS + 100)];

    let rendered = truncate_status_panel_sections(sections);

    assert!(rendered.chars().count() <= STATUS_PANEL_MAX_CHARS);
    assert!(!rendered.contains("계속 처리 중"));
}

#[test]
fn status_panel_heartbeat_without_new_events_is_stable() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(176);

    let first = events.render_status_panel_with_heartbeat(
        channel_id,
        &ProviderKind::Codex,
        1_700_000_000,
        1_700_000_005,
    );
    let second = events.render_status_panel_with_heartbeat(
        channel_id,
        &ProviderKind::Codex,
        1_700_000_000,
        1_700_000_010,
    );

    assert_eq!(first, second);
    assert!(!second.contains("계속 처리 중"));
}

#[test]
fn status_panel_turn_completed_after_monitor_wait_renders_background_completion() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(172);
    events.push_status_event(channel_id, StatusEvent::MonitorWait);
    events.push_status_event(channel_id, StatusEvent::TurnCompleted { background: true });

    assert_eq!(
        status_for(&events, channel_id),
        DerivedStatus::Completed {
            kind: CompletedKind::Background
        }
    );
    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.starts_with("✅ **백그라운드 완료** — claude"));
    assert!(!rendered.contains("💤 monitor 대기"));
}

#[test]
fn status_panel_turn_completed_after_aborted_tool_renders_terminal_completion() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(173);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use("Task", &json!({"description": "Investigate"}).to_string()),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result(Some("Task"), true),
    );
    events.push_status_event(channel_id, StatusEvent::TurnCompleted { background: false });

    assert_eq!(
        status_for(&events, channel_id),
        DerivedStatus::Completed {
            kind: CompletedKind::Foreground
        }
    );
    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.starts_with("✅ **응답 완료** — claude"));
    assert!(!rendered.contains("🟢 진행 중"));
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
    assert!(rendered.contains("기존 세션 복원"));
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
    assert!(fresh.contains("🆕 새 세션 시작"));
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
fn status_panel_appends_recovery_message_count_line_when_present() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(1781);
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        "session_fresh",
        &json!({
            "reason": "idle_timeout",
            "recoveryMessageCount": 7,
        }),
    ));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("🆕 새 세션 시작"));
    assert!(rendered.contains("(최근 대화 7개를 읽어들였습니다)"));
}

/// #3055 — a watcher-direct turn that has no session lifecycle event of its own
/// must not reuse the per-channel session panel snapshot left behind by a prior
/// turn's `session_fresh(recoveryMessageCount=N)`. The watcher completion path
/// re-derives the snapshot for the current turn; when the current turn has no
/// lifecycle row it clears the panel (mirroring the bridge), so the stale
/// `🆕 새 세션 시작 (최근 대화 N개…)` line is gone.
#[test]
fn status_panel_clears_stale_session_line_for_watcher_turn_without_lifecycle() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3055);

    // Turn A: a fresh-session/recovery event sets the channel-scoped snapshot.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        "session_fresh",
        &json!({
            "reason": "no_cached_provider_session",
            "recoveryMessageCount": 33,
        }),
    ));
    let turn_a = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(turn_a.contains("🆕 새 세션 시작"));
    assert!(turn_a.contains("(최근 대화 33개를 읽어들였습니다)"));

    // Turn B (watcher-direct, no lifecycle row): the completion path clears the
    // session panel before rendering. The cleared snapshot must drop the stale
    // new-session/recovery line.
    assert!(events.clear_session_panel(channel_id));
    let turn_b = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(!turn_b.contains("🆕 새 세션 시작"));
    assert!(!turn_b.contains("최근 대화"));

    // Idempotent: clearing an already-cleared panel reports no change.
    assert!(!events.clear_session_panel(channel_id));
}

#[test]
fn status_panel_omits_recovery_line_when_count_is_zero_or_missing() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(1782);
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        "session_fresh",
        &json!({
            "reason": "idle_timeout",
            "recoveryMessageCount": 0,
        }),
    ));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("🆕 새 세션 시작"));
    assert!(!rendered.contains("최근 대화"));

    let other_channel = ChannelId::new(1783);
    assert!(events.set_session_panel_lifecycle_event(
        other_channel,
        "session_fresh",
        &json!({ "reason": "first_turn" }),
    ));
    let rendered = events.render_status_panel(other_channel, &ProviderKind::Claude, 1_700_000_000);
    assert!(!rendered.contains("최근 대화"));
}

#[test]
fn status_panel_omits_session_line_when_lifecycle_details_are_absent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(180);
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        "session_fresh",
        &json!({
            "reason": "idle_timeout",
            "recoveryMessageCount": 25,
        }),
    ));
    assert!(
        events
            .render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000)
            .contains("🆕 새 세션 시작")
    );

    assert!(events.set_session_panel_lifecycle_event(channel_id, "session_resumed", &json!({}),));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(!rendered.contains("Lifecycle "));
    assert!(!rendered.contains("새 세션 시작"));
    assert!(!rendered.contains("최근 대화"));
}

#[test]
fn status_panel_omits_context_line_when_token_data_is_absent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(181);

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    assert!(!rendered.contains("Context   "));
}

#[test]
fn recent_header_prefers_dispatch_owner_over_local_node() {
    let snapshot = TaskPanelSnapshot {
        dispatch_id: "dsp_55".to_string(),
        card_id: None,
        dispatch_type: None,
        owner_instance_id: Some("mac-book-release".to_string()),
        card_title: None,
        dispatch_title: None,
        github_issue_number: None,
    };
    assert_eq!(
        render_recent_section_header(Some(&snapshot), true, Some("mac-mini-release")),
        "🖥️ Recent (mac-book-release)"
    );
}

#[test]
fn recent_header_falls_back_to_local_node_when_no_dispatch_owner() {
    assert_eq!(
        render_recent_section_header(None, true, Some("mac-mini-release")),
        "🖥️ Recent (mac-mini-release)"
    );
    let snapshot_without_owner = TaskPanelSnapshot {
        dispatch_id: "dsp_99".to_string(),
        card_id: None,
        dispatch_type: None,
        owner_instance_id: None,
        card_title: None,
        dispatch_title: None,
        github_issue_number: None,
    };
    assert_eq!(
        render_recent_section_header(
            Some(&snapshot_without_owner),
            true,
            Some("mac-mini-release")
        ),
        "🖥️ Recent (mac-mini-release)"
    );
}

#[test]
fn recent_header_omits_node_when_cluster_disabled_or_unknown() {
    let snapshot = TaskPanelSnapshot {
        dispatch_id: "dsp_55".to_string(),
        card_id: None,
        dispatch_type: None,
        owner_instance_id: Some("mac-book-release".to_string()),
        card_title: None,
        dispatch_title: None,
        github_issue_number: None,
    };
    assert_eq!(
        render_recent_section_header(Some(&snapshot), false, Some("mac-mini-release")),
        "🖥️ Recent"
    );
    assert_eq!(render_recent_section_header(None, true, None), "🖥️ Recent");
}

#[test]
fn status_panel_renders_task_line_with_card_title() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(185);
    assert!(events.set_task_panel_info(
        channel_id,
        TaskPanelInfo {
            dispatch_id: "bddc480d-43d1-4f1f-b3fd-e0d96b3b3d82",
            card_id: Some("e781f0c4-ea65-4dc3-814a-279d6eecadac"),
            dispatch_type: Some("review"),
            card_title: Some("Resolve runtime maintenance issues"),
            ..Default::default()
        },
    ));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Codex, 1_700_000_000);

    assert!(
        rendered.contains("Task      review · \"Resolve runtime maintenance issues\" · #bddc480d")
    );
    assert!(!rendered.contains("card #"));
    assert!(!rendered.contains("e781f0c4"));
}

#[test]
fn status_panel_renders_task_line_with_github_issue() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(188);
    assert!(events.set_task_panel_info(
        channel_id,
        TaskPanelInfo {
            dispatch_id: "bddc480d-43d1-4f1f-b3fd-e0d96b3b3d82",
            card_id: Some("card-xyz"),
            dispatch_type: Some("review"),
            card_title: Some("Fix CI inventory drift"),
            github_issue_number: Some(1234),
            ..Default::default()
        },
    ));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    assert!(
        rendered.contains("Task      review · gh#1234 \"Fix CI inventory drift\" · dsp #bddc480d")
    );
}

#[test]
fn status_panel_falls_back_to_dispatch_title_when_card_title_missing() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(189);
    assert!(events.set_task_panel_info(
        channel_id,
        TaskPanelInfo {
            dispatch_id: "dsp_abcdef12345",
            dispatch_type: Some("implementation"),
            dispatch_title: Some("Backfill outbox claims"),
            ..Default::default()
        },
    ));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    assert!(
        rendered.contains("Task      implementation · \"Backfill outbox claims\" · #dsp\\_abcd")
    );
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
    assert!(events.set_task_panel_info(
        channel_id,
        TaskPanelInfo {
            dispatch_id: "dsp_404",
            ..Default::default()
        },
    ));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    assert!(rendered.contains("Task      dispatch #dsp\\_404"));
    assert!(!rendered.contains("card #"));
}

#[test]
fn status_panel_renders_context_usage_severity_levels() {
    let events = PlaceholderLiveEvents::default();
    let normal_channel_id = ChannelId::new(182);
    assert!(events.set_context_panel_usage(normal_channel_id, None, 740, 0, 0, 1000, 90));
    let normal =
        events.render_status_panel(normal_channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(normal.contains("Context   📦 740 / 1.0k tokens (74%) · auto-compact 90%"));
    assert!(!normal.contains("임박"));
    assert!(!normal.contains("자동 압축 직전"));

    let approaching_channel_id = ChannelId::new(183);
    events.set_context_panel_usage(approaching_channel_id, None, 700, 40, 10, 1000, 90);
    let approaching =
        events.render_status_panel(approaching_channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(approaching.contains("Context   📦 750 / 1.0k tokens (75%) · auto-compact 90% (임박)"));

    let critical_channel_id = ChannelId::new(184);
    events.set_context_panel_usage(critical_channel_id, None, 700, 100, 50, 1000, 90);
    let critical =
        events.render_status_panel(critical_channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(
        critical
            .contains("Context   ⚠️ 850 / 1.0k tokens (85%) · auto-compact 90% — 자동 압축 직전")
    );
}

#[test]
fn status_panel_caps_context_usage_display_at_100_percent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(188);
    assert!(events.set_context_panel_usage(channel_id, None, 4000, 80, 10, 1000, 60));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    assert!(rendered.contains("Context   ⚠️ 4.1k / 1.0k tokens (100%) · auto-compact 60%"));
    assert!(!rendered.contains("(409%)"));
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
fn status_panel_tracks_codex_update_plan_items() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(785);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "update_plan",
            &json!({
                "plan": [
                    {"step": "Inspect wrapper events", "status": "completed"},
                    {"step": "Render Codex plan", "status": "in_progress"}
                ]
            })
            .to_string(),
        ),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Codex, 1_700_000_000);
    assert!(rendered.contains("Plan"));
    assert!(rendered.contains("- [x] Inspect wrapper events"));
    assert!(rendered.contains("- [ ] Render Codex plan"));
    assert!(!rendered.contains("Subagents"));
}

#[test]
fn status_panel_tracks_codex_update_plan_from_bridge_stringified_arguments() {
    for (idx, name) in ["update_plan", "updateplan"].into_iter().enumerate() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(786 + idx as u64);
        let modern_arguments = json!({
            "plan": [
                {"step": "Read modern Codex function call", "status": "completed"},
                {"step": "Render bridge plan", "status": "in_progress"}
            ]
        })
        .to_string();
        let bridge_input = serde_json::to_string_pretty(&json!(modern_arguments)).unwrap();

        events.push_status_events(channel_id, status_events_from_tool_use(name, &bridge_input));

        let rendered = events.render_status_panel(channel_id, &ProviderKind::Codex, 1_700_000_000);
        assert!(rendered.contains("Plan"), "{name} rendered:\n{rendered}");
        assert!(
            rendered.contains("- [x] Read modern Codex function call"),
            "{name} rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("- [ ] Render bridge plan"),
            "{name} rendered:\n{rendered}"
        );
        assert!(
            !rendered.contains("Subagents"),
            "{name} rendered:\n{rendered}"
        );
    }
}

#[test]
fn recent_events_skip_task_tool_family_represented_by_tasks_section() {
    assert!(
        RecentPlaceholderEvent::tool_use(
            "TaskCreate",
            &json!({"subject": "Create grouped Tasks section"}).to_string(),
        )
        .is_none()
    );
    assert!(
        RecentPlaceholderEvent::tool_use(
            "TaskUpdate",
            &json!({"taskId": "task-1", "status": "completed"}).to_string(),
        )
        .is_none()
    );
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
fn status_events_from_json_captures_workflow_progress_array() {
    let events = status_events_from_json(&json!({
        "type": "system",
        "subtype": "task_progress",
        "task_id": "wf-1",
        "summary": "probe",
        "workflow_progress": [
            {"type": "workflow_phase", "index": 1, "title": "P1"},
            {
                "type": "workflow_agent",
                "index": 1,
                "label": "pinger",
                "phaseIndex": 1,
                "phaseTitle": "P1",
                "state": "progress"
            }
        ]
    }));

    assert_eq!(
        events,
        vec![
            StatusEvent::WorkflowPhase {
                task_id: Some("wf-1".to_string()),
                index: 1,
                title: "P1".to_string()
            },
            StatusEvent::WorkflowAgent {
                task_id: Some("wf-1".to_string()),
                index: 1,
                label: "pinger".to_string(),
                phase_index: Some(1),
                phase_title: Some("P1".to_string()),
                state: "progress".to_string()
            }
        ]
    );
}

#[test]
fn status_events_from_json_captures_top_level_workflow_events() {
    assert_eq!(
        status_events_from_json(&json!({
            "type": "workflow_phase",
            "taskId": "wf-1",
            "index": 2,
            "title": "Implement"
        })),
        vec![StatusEvent::WorkflowPhase {
            task_id: Some("wf-1".to_string()),
            index: 2,
            title: "Implement".to_string(),
        }]
    );

    assert_eq!(
        status_events_from_json(&json!({
            "type": "workflow_agent",
            "task_id": "wf-1",
            "index": 3,
            "label": "reviewer",
            "phase_index": 2,
            "phase_title": "Implement",
            "status": "running"
        })),
        vec![StatusEvent::WorkflowAgent {
            task_id: Some("wf-1".to_string()),
            index: 3,
            label: "reviewer".to_string(),
            phase_index: Some(2),
            phase_title: Some("Implement".to_string()),
            state: "running".to_string(),
        }]
    );

    assert_eq!(
        status_events_from_json(&json!({
            "type": "workflow_log",
            "workflowRunId": "wf-1",
            "message": "review started"
        })),
        vec![StatusEvent::WorkflowLog {
            task_id: Some("wf-1".to_string()),
            summary: "review started".to_string(),
        }]
    );
}

#[test]
fn status_panel_tracks_workflow_phase_agents() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(2894);
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": "wf-1",
            "task_type": "local_workflow",
            "workflow_name": "probe"
        })),
    );
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "system",
            "subtype": "task_progress",
            "task_id": "wf-1",
            "workflow_progress": [
                {"type": "workflow_phase", "index": 1, "title": "P1"},
                {
                    "type": "workflow_agent",
                    "index": 1,
                    "label": "pinger",
                    "phaseIndex": 1,
                    "phaseTitle": "P1",
                    "state": "done"
                }
            ]
        })),
    );
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": "wf-1",
            "status": "completed",
            "summary": "Dynamic workflow \"probe\" completed"
        })),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("Workflow"));
    assert!(rendered.contains("probe"));
    assert!(rendered.contains("P1: pinger ✓"));
    assert!(rendered.contains("Dynamic workflow"));
    assert!(rendered.chars().count() <= STATUS_PANEL_MAX_CHARS);
}

#[test]
fn status_panel_caps_partial_workflow_state_without_start() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(2895);
    for idx in 0..=STATUS_PANEL_WORKFLOW_LIMIT {
        events.push_status_events(
            channel_id,
            status_events_from_json(&json!({
                "type": "workflow_phase",
                "task_id": format!("wf-{idx}"),
                "index": 1,
                "title": format!("phase {idx}")
            })),
        );
    }

    let status_entry = events
        .status_by_channel
        .get(&channel_id)
        .expect("status panel state");
    let guard = status_entry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    assert_eq!(
        guard.workflows.len(),
        STATUS_PANEL_WORKFLOW_LIMIT,
        "partial workflow events cap stored workflow slots at the visible workflow limit"
    );
    assert_eq!(
        guard
            .workflows
            .first()
            .and_then(|slot| slot.task_id.as_deref()),
        Some("wf-1")
    );
    assert_eq!(
        guard
            .workflows
            .last()
            .and_then(|slot| slot.task_id.as_deref()),
        Some("wf-5")
    );
    drop(guard);

    let channel_id = ChannelId::new(2896);
    for idx in 0..=STATUS_PANEL_WORKFLOW_PHASE_LIMIT {
        events.push_status_events(
            channel_id,
            status_events_from_json(&json!({
                "type": "workflow_phase",
                "task_id": "wf-partial",
                "index": idx,
                "title": format!("phase {idx}")
            })),
        );
    }
    for idx in 0..=STATUS_PANEL_WORKFLOW_AGENT_LIMIT {
        events.push_status_events(
            channel_id,
            status_events_from_json(&json!({
                "type": "workflow_agent",
                "task_id": "wf-partial",
                "index": idx,
                "label": format!("agent {idx}"),
                "phaseIndex": idx,
                "phaseTitle": format!("phase {idx}"),
                "state": "progress"
            })),
        );
    }

    let status_entry = events
        .status_by_channel
        .get(&channel_id)
        .expect("status panel state");
    let guard = status_entry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let slot = guard.workflows.first().expect("partial workflow slot");

    assert_eq!(
        slot.phases.len(),
        STATUS_PANEL_WORKFLOW_PHASE_LIMIT,
        "partial workflow phases are capped at ten stored rows"
    );
    assert_eq!(slot.phases.first().map(|phase| phase.index), Some(1));
    assert_eq!(
        slot.phases.last().map(|phase| phase.index),
        Some(STATUS_PANEL_WORKFLOW_PHASE_LIMIT as u64)
    );
    assert_eq!(
        slot.agents.len(),
        STATUS_PANEL_WORKFLOW_AGENT_LIMIT,
        "partial workflow agents are capped at ten stored rows"
    );
    assert_eq!(slot.agents.first().map(|agent| agent.index), Some(1));
    assert_eq!(
        slot.agents.last().map(|agent| agent.index),
        Some(STATUS_PANEL_WORKFLOW_AGENT_LIMIT as u64)
    );
}

#[test]
fn status_panel_keeps_latest_ten_subagents() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(180);
    for idx in 0..=10 {
        events.push_status_events(
            channel_id,
            status_events_from_tool_use(
                "Task",
                &json!({
                    "subagent_type": "explorer",
                    "description": format!("subagent {idx}")
                })
                .to_string(),
            ),
        );
    }

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let status_entry = events
        .status_by_channel
        .get(&channel_id)
        .expect("status panel state");
    let guard = status_entry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    assert_eq!(guard.subagents.len(), STATUS_PANEL_SUBAGENT_LIMIT);
    assert_eq!(
        guard.subagents.first().map(|slot| slot.desc.as_str()),
        Some("subagent 1")
    );
    assert_eq!(
        guard.subagents.last().map(|slot| slot.desc.as_str()),
        Some("subagent 10")
    );
    assert!(!rendered.contains("explorer subagent 0"));
    assert!(rendered.contains("explorer subagent 1"));
    assert!(rendered.contains("explorer subagent 10"));
}

#[test]
fn status_panel_stays_within_plain_content_limit() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(181);

    let todos = (0..STATUS_PANEL_TODO_LIMIT)
        .map(|idx| {
            json!({
                "content": format!("todo {idx} {}", "x".repeat(200)),
                "status": "in_progress"
            })
        })
        .collect::<Vec<_>>();
    events.push_status_events(
        channel_id,
        status_events_from_tool_use("TodoWrite", &json!({ "todos": todos }).to_string()),
    );
    events.set_task_panel_info(
        channel_id,
        TaskPanelInfo {
            dispatch_id: "1234567890abcdef",
            card_id: Some("CARD-123"),
            dispatch_type: Some("issue"),
            owner_instance_id: Some("mac-book-release"),
            card_title: Some(
                "status panel plain-content limit regression guard with a deliberately long task title",
            ),
            dispatch_title: None,
            github_issue_number: Some(2891),
        },
    );
    events.set_context_panel_usage(
        channel_id,
        Some("session-123456789"),
        85_000,
        15_000,
        5_000,
        100_000,
        60,
    );

    for idx in 0..STATUS_PANEL_SUBAGENT_LIMIT {
        events.push_status_events(
            channel_id,
            status_events_from_tool_use(
                "Task",
                &json!({
                    "subagent_type": "explorer",
                    "description": format!("subagent {idx} {}", "d".repeat(180))
                })
                .to_string(),
            ),
        );
        events.push_status_events(
            channel_id,
            status_events_from_task_notification(
                "subagent",
                "running",
                &format!("recent {idx} {}", "r".repeat(180)),
            ),
        );
    }

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    assert!(
        rendered.chars().count() <= STATUS_PANEL_MAX_CHARS,
        "status panel exceeded Discord plain-content limit: {}",
        rendered.chars().count()
    );
}

#[test]
fn status_panel_renders_taskcreate_in_tasks_after_ack() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(81);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "TaskCreate",
            &json!({"subject": "Stream parser extraction layer"}).to_string(),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result(Some("TaskCreate"), false),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("Tasks"));
    assert!(rendered.contains("TaskCreate Stream parser extraction layer"));
    assert!(!rendered.contains("Subagents"));
}

#[test]
fn status_panel_does_not_render_taskcreate_as_subagent_on_unrelated_tool_end() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(82);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "TaskCreate",
            &json!({"subject": "Turn bridge integration"}).to_string(),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result(Some("Read"), false),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("Tasks"));
    assert!(rendered.contains("TaskCreate Turn bridge integration"));
    assert!(!rendered.contains("Subagents"));
}

#[test]
fn status_panel_taskupdate_updates_existing_task_by_id() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(83);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "TaskCreate",
            &json!({"taskId": "task-1", "subject": "Wire Tasks panel"}).to_string(),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "TaskUpdate",
            &json!({"taskId": "task-1", "status": "completed"}).to_string(),
        ),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let task_lines = rendered
        .lines()
        .filter(|line| line.starts_with("└ Task"))
        .collect::<Vec<_>>();
    assert_eq!(task_lines.len(), 1, "rendered:\n{rendered}");
    assert!(rendered.contains("TaskUpdate task-1 · Wire Tasks panel · completed"));
}

#[test]
fn status_panel_keeps_latest_task_tool_entries_under_cap() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(84);
    for idx in 0..=STATUS_PANEL_TASK_LIMIT {
        events.push_status_events(
            channel_id,
            status_events_from_tool_use(
                "TaskCreate",
                &json!({
                    "taskId": format!("task-{idx}"),
                    "subject": format!("task subject {idx}")
                })
                .to_string(),
            ),
        );
    }

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let status_entry = events
        .status_by_channel
        .get(&channel_id)
        .expect("status panel state");
    let guard = status_entry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    assert_eq!(guard.tasks.len(), STATUS_PANEL_TASK_LIMIT);
    assert_eq!(
        guard.tasks.first().and_then(|slot| slot.task_id.as_deref()),
        Some("task-1")
    );
    assert_eq!(
        guard.tasks.last().and_then(|slot| slot.task_id.as_deref()),
        Some("task-10")
    );
    assert!(!rendered.contains("task subject 0"));
    assert!(rendered.contains("task subject 1"));
    assert!(rendered.contains("task subject 10"));
}

#[test]
fn status_panel_renders_plan_but_hides_subagents_for_codex() {
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
    assert!(rendered.contains("Plan"));
    assert!(rendered.contains("Hidden for Codex"));
    assert!(!rendered.contains("Subagents"));
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
        status_events_from_tool_result(Some("TaskCreate"), false),
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

#[test]
fn tool_use_toolsearch_pretty_json_renders_query_not_bare_brace() {
    // #2847: tool input arrives as serde_json::to_string_pretty (multi-line).
    // Before the fix the live-event line collapsed to a bare "[ToolSearch] {".
    let pretty = serde_json::to_string_pretty(&json!({
        "query": "select:Read,Edit",
        "max_results": 5
    }))
    .unwrap();
    let line = RecentPlaceholderEvent::tool_use("ToolSearch", &pretty)
        .expect("non-empty summary")
        .render_line();
    assert!(line.starts_with("[ToolSearch]"), "got: {line}");
    assert!(line.contains("select:Read,Edit"), "got: {line}");
    assert!(line.contains("limit 5"), "got: {line}");
    assert_ne!(line.trim(), "[ToolSearch] {");
    assert!(!line.trim_end().ends_with('{'), "got: {line}");
}

#[test]
fn tool_use_monitor_pretty_json_renders_summary_not_bare_brace() {
    let pretty = serde_json::to_string_pretty(&json!({
        "description": "watch CI for PR 2850",
        "command": "gh pr checks 2850"
    }))
    .unwrap();
    let line = RecentPlaceholderEvent::tool_use("Monitor", &pretty)
        .expect("non-empty summary")
        .render_line();
    assert!(line.starts_with("[Monitor]"), "got: {line}");
    assert!(line.contains("watch CI for PR 2850"), "got: {line}");
    assert_ne!(line.trim(), "[Monitor] {");
    assert!(!line.trim_end().ends_with('{'), "got: {line}");
}

#[test]
fn tool_use_unknown_pretty_json_falls_back_to_compact_not_brace() {
    // The default arm now compacts pretty JSON instead of leaking a "{" line.
    let pretty = serde_json::to_string_pretty(&json!({ "some_field": "value" })).unwrap();
    let line = RecentPlaceholderEvent::tool_use("SomeUnknownTool", &pretty)
        .expect("non-empty summary")
        .render_line();
    assert!(!line.trim_end().ends_with('{'), "got: {line}");
    assert!(line.contains("some_field"), "got: {line}");
}

#[test]
fn status_events_toolsearch_pretty_json_args_summary_not_bare_brace() {
    // #2847: the status-panel path (status_events_from_tool_use) shares the same
    // format_tool_input fix, so the ToolStart args summary is no longer "{".
    let pretty = serde_json::to_string_pretty(&json!({
        "query": "Monitor schema",
        "max_results": 3
    }))
    .unwrap();
    let events = status_events_from_tool_use("ToolSearch", &pretty);
    let StatusEvent::ToolStart { args_summary, .. } = &events[0] else {
        panic!("expected ToolStart, got {:?}", events[0]);
    };
    let summary = args_summary.as_deref().unwrap_or("");
    assert!(summary.contains("Monitor schema"), "got: {summary}");
    assert_ne!(summary.trim(), "{");
    assert!(!summary.trim_end().ends_with('{'), "got: {summary}");
}
