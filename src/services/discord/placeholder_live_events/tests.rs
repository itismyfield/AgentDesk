use super::super::formatting::{
    MonitorHandoffReason, MonitorHandoffStatus, build_monitor_handoff_placeholder_with_live_events,
    build_processing_status_block, build_streaming_placeholder_text, plan_streaming_rollover,
    redact_sensitive_for_placeholder,
};
use super::common::{
    EVENT_BLOCK_MAX_CHARS, EVENT_LINE_MAX_CHARS, STATUS_PANEL_MAX_CHARS, STATUS_PANEL_TASK_LIMIT,
};
use super::*;
use serde_json::json;

#[test]
fn render_block_compacts_newest_events_under_limit() {
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
    assert!(block.chars().count() <= EVENT_BLOCK_MAX_CHARS);
    let live_lines = block
        .lines()
        .filter(|line| line.starts_with("• [Bash]"))
        .collect::<Vec<_>>();
    assert_eq!(live_lines.len(), 1);
    assert!(block.contains("5회"));
    assert!(!block.contains("echo 24"));
}

#[test]
fn raw_debug_block_keeps_newest_events_under_limit() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(43);
    for idx in 0..25 {
        events.push_event(
            channel_id,
            RecentPlaceholderEvent::tool_use("Bash", &format!(r#"{{"command":"echo {idx}"}}"#))
                .unwrap(),
        );
    }

    let block = events.render_raw_block_for_tests(channel_id).unwrap();
    assert!(block.starts_with("```text\n"));
    assert!(block.ends_with("\n```"));
    assert!(block.chars().count() <= EVENT_BLOCK_MAX_CHARS);
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

    let block = events.render_raw_block_for_tests(channel_id).unwrap();
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

    let compact_block = events.render_block(channel_id).unwrap();
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
        Some(&compact_block),
    );

    assert!(
        rendered.len() <= 4096,
        "monitor handoff placeholder exceeded embed description limit: {}",
        rendered.len()
    );
    assert!(rendered.contains("[Bash]"));
    assert!(!rendered.contains("```text"));
    assert!(!rendered.contains("secret-token"));
    assert!(!rendered.contains("api_key=secret"));
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
    assert!(
        !rendered.contains("cargo test"),
        "status header should show the tool class, not raw command text: {rendered}"
    );
    assert!(rendered.chars().count() <= STATUS_PANEL_MAX_CHARS);
}

#[test]
fn status_panel_recent_compacts_raw_command_details() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(78);
    let raw_command = "cargo test --lib placeholder_live_events -- --nocapture";
    let tool_args = json!({"command": raw_command}).to_string();
    events.push_status_events(channel_id, status_events_from_tool_use("Bash", &tool_args));
    events.push_event(
        channel_id,
        RecentPlaceholderEvent::tool_use("Bash", &tool_args).unwrap(),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    // #3983 item 5a: the footer no longer echoes the compact 🖥️ Recent block; the
    // activity label shows the tool class, never the raw command detail.
    assert!(rendered.contains("🔧 도구 실행 중"));
    assert!(!rendered.contains("🖥️ Recent"));
    assert!(!rendered.contains("```text"));
    assert!(
        !rendered.contains(raw_command),
        "normal status panel must not render raw command detail: {rendered}"
    );

    let raw_debug_block = events.render_raw_block_for_tests(channel_id).unwrap();
    assert!(
        raw_debug_block.contains(raw_command),
        "explicit debug render keeps raw detail available outside the normal status panel"
    );
}

#[test]
fn characterize_rollover_seed_has_no_status_panel_content_s0() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3089);
    let tool_args = json!({"command": "cargo test --lib placeholder_live_events"}).to_string();
    events.push_status_events(channel_id, status_events_from_tool_use("Bash", &tool_args));
    events.push_event(
        channel_id,
        RecentPlaceholderEvent::tool_use("Bash", &tool_args).unwrap(),
    );

    let panel = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(panel.contains("도구 실행 중"));
    assert!(panel.contains("[Bash]"));
    assert!(!panel.contains("cargo test --lib placeholder_live_events"));

    let status_block = build_processing_status_block("⠸");
    let current_portion = "relay body ".repeat(250);
    let plan = plan_streaming_rollover(&current_portion, &status_block)
        .expect("representative relay body should roll over");
    let rollover_seed = build_streaming_placeholder_text("", &status_block);

    assert_eq!(rollover_seed, status_block);
    assert!(
        plan.display_snapshot
            .ends_with(&format!("\n\n{status_block}"))
    );
    for status_panel_fragment in [
        "도구 실행 중",
        "[Bash]",
        "cargo test --lib placeholder_live_events",
    ] {
        assert!(!rollover_seed.contains(status_panel_fragment));
        assert!(!plan.display_snapshot.contains(status_panel_fragment));
        assert!(!plan.frozen_chunk.contains(status_panel_fragment));
    }
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
    assert!(rendered.starts_with("✅ 완료"));
    assert!(!rendered.contains("🟢 진행 중"));
}

#[test]
fn status_panel_absorbs_stale_and_final_into_the_activity_emoji() {
    // #3983 items 2 + B: the separate 신뢰도 line is retired; the freshness class is
    // absorbed into the line-1 activity emoji, and line 2 carries the time line.
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(38120);

    // Running turn → 🟢 activity + `마지막 업데이트`/`턴 시작` time line, no 신뢰도.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use("Bash", &json!({"command": "cargo test"}).to_string()),
    );
    let live = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(
        live.contains("🔧 도구 실행 중"),
        "running activity: {live:?}"
    );
    assert!(
        live.contains("마지막 업데이트 : <t:") && live.contains("턴 시작 : <t:"),
        "time line must render: {live:?}"
    );
    assert!(
        !live.contains("신뢰도"),
        "confidence line is retired: {live:?}"
    );

    // Completion → `✅ 완료` (final absorbed into the emoji).
    events.push_status_event(channel_id, StatusEvent::TurnCompleted { background: false });
    let done = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(done.starts_with("✅ 완료"), "final activity: {done:?}");
    assert!(!done.contains("신뢰도"));

    // Answer relayed but session-end unconfirmed → `🟡 응답 지연 · 조사 권장`.
    let stale = events.render_terminal_ui_obligation_panel(
        channel_id,
        &ProviderKind::Claude,
        1_700_000_000,
        TerminalUiObligationPanelStatus::Deadline,
    );
    assert!(
        stale.starts_with("🟡 응답 지연 · 조사 권장"),
        "stale is absorbed into the 🟡 activity emoji: {stale:?}"
    );

    // Delivery done, termination still confirming → the pending `↻` activity.
    let pending = events.render_terminal_ui_obligation_panel(
        channel_id,
        &ProviderKind::Claude,
        1_700_000_000,
        TerminalUiObligationPanelStatus::Pending,
    );
    assert!(
        pending.starts_with("↻ 응답 전달됨 · 세션 종료 확인 중"),
        "pending activity: {pending:?}"
    );
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
    assert!(rendered.contains("[Bash]"));
    // #3983 item 5a: the 🖥️ Recent echo is retired from the footer.
    assert!(!rendered.contains("🖥️ Recent"));
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
    assert!(rendered.starts_with("✅ 백그라운드 완료"));
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
    assert!(rendered.starts_with("✅ 완료"));
    assert!(!rendered.contains("🟢 진행 중"));
}

#[test]
fn status_panel_renders_session_resumed_line_from_lifecycle_details() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(177);
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        None,
        "session_resumed",
        &json!({
            "provider_session_id": "8f21abcd12345678",
            "tmux_reused": true
        }),
    ));

    // #3983 item4: the session line is NO LONGER in the every-tick footer.
    let footer = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(!footer.contains("기존 세션 복원"));

    // It is emitted once, at the top, via the one-shot banner claim.
    let banner = events
        .claim_session_banner_line(channel_id, &ProviderKind::Claude)
        .expect("first claim yields the one-shot session banner");
    assert!(banner.contains("기존 세션 복원"));
    assert!(banner.contains("provider session claude#8f21abcd…"));
    assert!(banner.contains("tmux kept"));
    assert!(!banner.contains("📋 세션 복원"));
    // Dedup: a second claim for the same session yields nothing.
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_none()
    );
}

/// #3087 — when a new session INSTANCE begins (a new tmux spawn → new
/// `.generation` mtime → new `session_instance_key`), the status panel must
/// drop the previous session's accumulated subagents and task-tool slots while
/// preserving the context/token usage snapshot. Then, re-firing the SAME
/// instance key + provider id must NOT wipe same-session accumulation (no
/// spurious reset on unrelated field churn).
#[test]
fn status_panel_resets_subagents_and_tasks_on_new_provider_session() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3087);

    // Session A: instance key A, established provider session, accumulate content.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch3087#100"),
        "session_resumed",
        &json!({ "provider_session_id": "session-A", "tmux_reused": true }),
    ));
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Inspect bridge"}).to_string(),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "TaskCreate",
            &json!({"taskId": "task-A", "subject": "session A task"}).to_string(),
        ),
    );
    assert!(events.set_context_panel_usage(channel_id, None, 4000, 80, 10, 1000, 60));

    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(guard.subagents.len(), 1);
        assert_eq!(guard.tasks.len(), 1);
        assert!(guard.context.is_some());
    }

    // Session B: a NEW spawn → NEW instance key (and a new provider id). Content
    // slots must be cleared, but the context/token snapshot must survive.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch3087#200"),
        "session_fresh",
        &json!({ "provider_session_id": "session-B", "tmux_reused": false }),
    ));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            guard.subagents.is_empty(),
            "subagents must reset on a new provider session"
        );
        assert!(
            guard.tasks.is_empty(),
            "tasks must reset on a new provider session"
        );
        assert!(
            guard.context.is_some(),
            "context/token usage must be preserved across the boundary"
        );
    }

    // Re-accumulate within session B, then re-fire the SAME instance key + id
    // (only unrelated field churn: tmux/recovery). The same-session slots must
    // be retained.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Session B work"}).to_string(),
        ),
    );
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch3087#200"),
        "session_fresh",
        &json!({ "provider_session_id": "session-B", "tmux_reused": true }),
    ));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            guard.subagents.len(),
            1,
            "same provider session must NOT reset accumulated subagents"
        );
        assert_eq!(
            guard.subagents.first().map(|slot| slot.desc.as_str()),
            Some("Session B work")
        );
    }
}

/// #3087 (codex P1 — false NON-reset) — a GENUINELY fresh session legitimately
/// arrives with `provider_session_id == None` (the common `/clear`,
/// idle-timeout, turn-cap, goal-fresh, `no_cached_provider_session` paths all
/// normalize to None). Such a fresh session is a NEW tmux spawn, so it carries a
/// NEW `session_instance_key` and MUST still reset the previous session's
/// accumulated subagents/tasks even though it has no provider id. Context/token
/// usage must be preserved across the boundary.
#[test]
fn status_panel_resets_on_fresh_session_with_no_provider_session_id() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(30871);

    // Prior session (instance key A): accumulate content + context.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch30871#100"),
        "session_resumed",
        &json!({ "provider_session_id": "stale-session", "tmux_reused": true }),
    ));
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Stale work"}).to_string(),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "TaskCreate",
            &json!({"taskId": "task-stale", "subject": "stale task"}).to_string(),
        ),
    );
    assert!(events.set_context_panel_usage(channel_id, None, 4000, 80, 10, 1000, 60));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(guard.subagents.len(), 1);
        assert_eq!(guard.tasks.len(), 1);
        assert!(guard.context.is_some());
    }

    // Fresh session = NEW spawn → NEW instance key, with NO provider session id
    // (e.g. /clear).
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch30871#200"),
        "session_fresh",
        &json!({ "reason": "no_cached_provider_session", "tmux_reused": false }),
    ));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            guard.subagents.is_empty(),
            "subagents must reset on a fresh session even when provider_session_id is None"
        );
        assert!(
            guard.tasks.is_empty(),
            "tasks must reset on a fresh session even when provider_session_id is None"
        );
        assert!(
            guard.context.is_some(),
            "context/token usage must be preserved across the fresh boundary"
        );
    }
}

/// #3087 (codex P1-A — false per-turn RESET) — the prior `turn_id`-keyed design
/// reset on EVERY status tick / turn of a no-provider-id session, because each
/// turn carries a new `turn_id`. The redesign keys the boundary on the STABLE
/// per-INSTANCE `session_instance_key` (the `.generation` spawn marker), which
/// is invariant across every tick AND every TURN of one session. This test
/// asserts that a no-provider-id session running MULTIPLE turns (the
/// `set_session_panel_*` lifecycle re-loaded each tick/turn, with field churn
/// modelling distinct turns) does NOT reset its mid-session accumulation.
#[test]
fn status_panel_preserves_accumulation_across_ticks_of_same_fresh_session() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(30872);
    // One spawn → one stable instance key across all of this session's turns.
    let instance_key = "AgentDesk-claude-ch30872#100";

    // First tick of the fresh session (no provider id) — establishes the marker.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some(instance_key),
        "session_fresh",
        &json!({ "reason": "idle_timeout", "tmux_reused": false }),
    ));

    // Accumulate mid-session content.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Mid-session work"}).to_string(),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "TaskCreate",
            &json!({"taskId": "task-mid", "subject": "mid task"}).to_string(),
        ),
    );

    // Subsequent TICKS AND TURNS of the SAME session instance: the same stable
    // instance key, still None provider id, only unrelated field churn (tmux /
    // recovery count). Even though these model >=2 distinct turns, the instance
    // key is unchanged, so there must be NO reset.
    for details in [
        json!({ "reason": "idle_timeout", "tmux_reused": true }),
        json!({ "reason": "idle_timeout", "tmux_reused": true, "recoveryMessageCount": 3 }),
        json!({ "reason": "no_cached_provider_session", "tmux_reused": true }),
    ] {
        events.set_session_panel_lifecycle_event(
            channel_id,
            Some(instance_key),
            "session_fresh",
            &details,
        );
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            guard.subagents.len(),
            1,
            "ticks/turns of the same session instance must NOT reset accumulated subagents"
        );
        assert_eq!(
            guard.subagents.first().map(|slot| slot.desc.as_str()),
            Some("Mid-session work")
        );
        assert_eq!(
            guard.tasks.len(),
            1,
            "ticks/turns of the same session instance must NOT reset accumulated tasks"
        );
    }
}

/// #3087 (codex P1-B — false mid-session RESET) — when a fresh session's
/// `provider_session_id` is assigned mid-session (`None`→`Some`, the
/// `StreamMessage::Init` handshake), the spawn marker is untouched, so the
/// `session_instance_key` is unchanged and the panel MUST NOT reset. This is
/// the case the old provider-id-delta gate got wrong; the redesign gates that
/// delta on the old id being `Some` too, so `None`→`Some` within one instance
/// is never a boundary.
#[test]
fn status_panel_preserves_accumulation_on_none_to_some_provider_id_same_instance() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(30874);
    let instance_key = "AgentDesk-claude-ch30874#100";

    // Fresh session begins with NO provider id yet.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some(instance_key),
        "session_fresh",
        &json!({ "reason": "first_turn", "tmux_reused": false }),
    ));
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Pre-init work"}).to_string(),
        ),
    );

    // Mid-session: the provider id is assigned (None→Some) on the SAME instance.
    // The lifecycle now renders `session_resumed` with the id, but the spawn
    // marker (instance key) is unchanged — there must be NO reset.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some(instance_key),
        "session_resumed",
        &json!({ "provider_session_id": "late-bound-id", "tmux_reused": true }),
    ));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            guard.subagents.len(),
            1,
            "None→Some provider id on the same instance must NOT reset accumulation"
        );
        assert_eq!(
            guard.subagents.first().map(|slot| slot.desc.as_str()),
            Some("Pre-init work")
        );
    }

    // A subsequent tick re-firing the SAME id on the SAME instance must also
    // not reset (idempotent within the instance).
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some(instance_key),
        "session_resumed",
        &json!({ "provider_session_id": "late-bound-id", "tmux_reused": true, "recoveryMessageCount": 2 }),
    ));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            guard.subagents.len(),
            1,
            "re-firing the same id on the same instance must NOT reset accumulation"
        );
    }
}

/// #3087 — two CONSECUTIVE distinct fresh sessions, both with
/// `provider_session_id == None` but different `session_instance_key`s (each a
/// new tmux spawn → new `.generation` mtime, e.g. back-to-back `/clear`s), must
/// EACH reset. The boundary is keyed on the instance-key change, so each new
/// spawn drops the prior session's accumulation.
#[test]
fn status_panel_resets_on_each_of_two_distinct_none_id_fresh_sessions() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(30873);

    // Fresh session #1 (instance key #1, no provider id) → accumulate.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch30873#100"),
        "session_fresh",
        &json!({ "reason": "first_turn", "tmux_reused": false }),
    ));
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Session 1 work"}).to_string(),
        ),
    );
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(guard.subagents.len(), 1);
    }

    // Fresh session #2 (new spawn → instance key #2, still no provider id) → reset.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch30873#200"),
        "session_fresh",
        &json!({ "reason": "goal_fresh", "tmux_reused": false }),
    ));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            guard.subagents.is_empty(),
            "a second distinct None-id fresh session must reset the first session's content"
        );
    }

    // Accumulate in session #2, then a THIRD distinct spawn → reset again.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Session 2 work"}).to_string(),
        ),
    );
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch30873#300"),
        "session_fresh",
        &json!({ "reason": "turn_cap", "tmux_reused": false }),
    ));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            guard.subagents.is_empty(),
            "a third distinct None-id fresh session must reset again"
        );
    }
}

/// #3087 (codex Edge 5 — false RESET on key AVAILABILITY transition) — the
/// `session_instance_key` can become available mid-session (`None`→`Some`)
/// purely because `tmux_session_name` resolved or the `.spawn_nonce` marker
/// became readable. That is NOT a session change. The boundary must be gated on
/// the OLD key being `Some` too (mirroring the provider-id gate), so a
/// `None`→`Some` key transition PRESERVES the same-session accumulation.
#[test]
fn status_panel_preserves_accumulation_on_none_to_some_instance_key_availability() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(30875);

    // First tick: no instance key yet (e.g. tmux name / nonce marker not yet
    // resolved). Establishes a session with a `None` key, same provider id.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        None,
        "session_resumed",
        &json!({ "provider_session_id": "stable-id", "tmux_reused": true }),
    ));
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Pre-key work"}).to_string(),
        ),
    );

    // Second tick of the SAME session: the instance key is now AVAILABLE
    // (`None`→`Some`), same provider id. This is an availability transition,
    // not a new session — there must be NO reset.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch30875#nonce-aaaa"),
        "session_resumed",
        &json!({ "provider_session_id": "stable-id", "tmux_reused": true }),
    ));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            guard.subagents.len(),
            1,
            "None→Some instance-key availability transition must NOT reset accumulation"
        );
        assert_eq!(
            guard.subagents.first().map(|slot| slot.desc.as_str()),
            Some("Pre-key work")
        );
    }

    // A subsequent genuinely-new spawn (key Some(a)→Some(b)) still resets.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch30875#nonce-bbbb"),
        "session_fresh",
        &json!({ "reason": "clear", "tmux_reused": false }),
    ));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            guard.subagents.is_empty(),
            "a Some(a)→Some(b) instance-key change (new spawn) must reset"
        );
    }
}

/// #3087 (codex Edge 3 — missing-nonce same-name respawn must NOT suppress a
/// real reset) — the prior mtime design folded a missing marker into a
/// `{name}#0` key, so two back-to-back respawns reusing the same tmux session
/// name both produced `{name}#0` → identical key → NO reset (the bug
/// persisted). The nonce design yields `None` when the marker is unavailable,
/// so a respawn whose nonce is missing never collides with a stored key; the
/// provider-session delta then remains the reset boundary instead of a
/// suppressed reset. This test models a same-name respawn where the FIRST
/// session had a real nonce and the respawn's nonce is unavailable (key `None`)
/// but the provider session genuinely changed — the panel MUST still reset.
#[test]
fn status_panel_resets_on_same_name_respawn_with_missing_nonce_via_provider_delta() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(30876);

    // Session #1: real nonce key + provider id A → accumulate.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch30876#nonce-1111"),
        "session_resumed",
        &json!({ "provider_session_id": "session-A", "tmux_reused": true }),
    ));
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Session A work"}).to_string(),
        ),
    );

    // Respawn reusing the SAME tmux name, but the `.spawn_nonce` marker is
    // unavailable this tick → instance key is `None`. The provider session
    // genuinely changed (A→B). Under the OLD mtime design both would key to
    // `{name}#0` and NOT reset; here the `None` key does not collide, and the
    // Some(a)→Some(b) provider delta drives the reset.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        None,
        "session_fresh",
        &json!({ "provider_session_id": "session-B", "tmux_reused": false }),
    ));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            guard.subagents.is_empty(),
            "same-name respawn with a missing nonce must STILL reset via the provider-session delta (no #0 collision)"
        );
    }
}

/// #3087 — two distinct spawns with distinct NONCES each reset (the positive
/// path: a respawn that DOES have a readable nonce changes the instance key, so
/// the reset fires even when there is no provider-session signal at all).
#[test]
fn status_panel_resets_on_two_distinct_nonces_without_provider_id() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(30877);

    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch30877#nonce-aaaa"),
        "session_fresh",
        &json!({ "reason": "first_turn", "tmux_reused": false }),
    ));
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Spawn 1 work"}).to_string(),
        ),
    );

    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch30877#nonce-bbbb"),
        "session_fresh",
        &json!({ "reason": "clear", "tmux_reused": false }),
    ));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            guard.subagents.is_empty(),
            "a distinct per-spawn nonce must reset even with no provider session id"
        );
    }
}

/// #3087 P2-2 — a TUI-direct spawn path (Claude-TUI / Codex-TUI) now stamps a
/// `.spawn_nonce` via `write_spawn_nonce`, exactly like the main provider spawn
/// sites. This drives the panel through the REAL filesystem chain those paths
/// use at runtime: each TUI-direct spawn writes a fresh nonce, the panel derives
/// its instance key from `session_panel_instance_key` (which reads that nonce),
/// and a second TUI-direct spawn mints a DISTINCT nonce → distinct instance key
/// → reset — even with no provider-session signal at all.
// Exercises `write_spawn_nonce` / `session_panel_instance_key`, which live in
// the unix-only `tmux` module (tmux/TUI-direct paths are unix-only).
#[cfg(unix)]
#[test]
fn status_panel_resets_across_two_tui_direct_spawns_via_stamped_nonce() {
    let _lock = match crate::config::shared_test_env_lock().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(30872);

    let tmux_name = format!(
        "AgentDesk-claude-tui-issue-3087-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    if let Some(parent) = std::path::Path::new(&crate::services::tmux_common::session_temp_path(
        &tmux_name,
        "spawn_nonce",
    ))
    .parent()
    {
        std::fs::create_dir_all(parent).expect("runtime dir");
    }

    // TUI-direct spawn #1 stamps a nonce (the exact call the Claude/Codex
    // TUI-direct paths now make right after `create_session`).
    crate::services::discord::write_spawn_nonce(&tmux_name).expect("tui spawn 1 nonce");
    let key1 = crate::services::discord::tmux::session_panel_instance_key(&tmux_name)
        .expect("tui spawn 1 must produce an instance key");
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some(&key1),
        "session_fresh",
        &json!({ "reason": "first_turn", "tmux_reused": false }),
    ));
    events.push_status_events(
        channel_id,
        status_events_from_tool_use(
            "Task",
            &json!({"subagent_type": "explorer", "description": "TUI spawn 1 work"}).to_string(),
        ),
    );

    // TUI-direct spawn #2 (same tmux name) mints a DISTINCT nonce → distinct key.
    crate::services::discord::write_spawn_nonce(&tmux_name).expect("tui spawn 2 nonce");
    let key2 = crate::services::discord::tmux::session_panel_instance_key(&tmux_name)
        .expect("tui spawn 2 must produce an instance key");
    assert_ne!(
        key1, key2,
        "a new TUI-direct spawn must change the instance key (fresh nonce)"
    );
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some(&key2),
        "session_fresh",
        &json!({ "reason": "clear", "tmux_reused": false }),
    ));
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            guard.subagents.is_empty(),
            "a fresh TUI-direct spawn (distinct stamped nonce) must reset the panel"
        );
    }

    unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
}

#[test]
fn status_panel_renders_session_fresh_and_fallback_distinctly() {
    let events = PlaceholderLiveEvents::default();
    let fresh_channel_id = ChannelId::new(178);
    events.set_session_panel_lifecycle_event(
        fresh_channel_id,
        None,
        "session_fresh",
        &json!({
            "reason": "first_turn",
            "provider_session_id": "fresh-session-id",
            "tmux_reused": false
        }),
    );

    // #3983 item4: session identity surfaces via the one-shot banner, not the footer.
    let fresh = events
        .claim_session_banner_line(fresh_channel_id, &ProviderKind::Codex)
        .expect("fresh session yields a one-shot banner");
    assert!(fresh.contains("🆕 새 세션 시작"));
    assert!(fresh.contains("provider session codex#fresh-se…"));
    assert!(fresh.contains("tmux new"));
    assert!(
        !events
            .render_status_panel(fresh_channel_id, &ProviderKind::Codex, 1_700_000_000)
            .contains("🆕 새 세션 시작")
    );

    let fallback_channel_id = ChannelId::new(179);
    events.set_session_panel_lifecycle_event(
        fallback_channel_id,
        None,
        "session_resume_failed_with_recovery",
        &json!({
            "reason": "resume_failed",
            "providerSessionId": "fallback-session-id",
            "tmuxStatus": "kept"
        }),
    );

    let fallback = events
        .claim_session_banner_line(fallback_channel_id, &ProviderKind::Claude)
        .expect("fallback session yields a one-shot banner");
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
        None,
        "session_fresh",
        &json!({
            "reason": "idle_timeout",
            "recoveryMessageCount": 7,
        }),
    ));

    // #3983 item4: the recovery-count line rides on the one-shot banner.
    let banner = events
        .claim_session_banner_line(channel_id, &ProviderKind::Claude)
        .expect("fresh session yields a one-shot banner");
    assert!(banner.contains("🆕 새 세션 시작"));
    assert!(banner.contains("(최근 대화 7개를 읽어들였습니다)"));
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
        None,
        "session_fresh",
        &json!({
            "reason": "no_cached_provider_session",
            "recoveryMessageCount": 33,
        }),
    ));
    // Turn A: the one-shot banner renders the fresh-session/recovery line once;
    // the every-tick footer never carries it (#3983 item4).
    let turn_a = events
        .claim_session_banner_line(channel_id, &ProviderKind::Claude)
        .expect("fresh session yields a one-shot banner");
    assert!(turn_a.contains("🆕 새 세션 시작"));
    assert!(turn_a.contains("(최근 대화 33개를 읽어들였습니다)"));
    assert!(
        !events
            .render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000)
            .contains("🆕 새 세션 시작")
    );

    // Turn B (watcher-direct, no lifecycle row): the completion path clears the
    // session panel. The cleared snapshot has no session to banner, so a
    // subsequent claim yields nothing — the stale new-session/recovery line can
    // never re-post.
    assert!(events.clear_session_panel(channel_id));
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_none()
    );
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
        None,
        "session_fresh",
        &json!({
            "reason": "idle_timeout",
            "recoveryMessageCount": 0,
        }),
    ));

    // #3983 item4: assert on the one-shot banner (the session line left the footer).
    let banner = events
        .claim_session_banner_line(channel_id, &ProviderKind::Claude)
        .expect("fresh session yields a one-shot banner");
    assert!(banner.contains("🆕 새 세션 시작"));
    assert!(!banner.contains("최근 대화"));

    let other_channel = ChannelId::new(1783);
    assert!(events.set_session_panel_lifecycle_event(
        other_channel,
        None,
        "session_fresh",
        &json!({ "reason": "first_turn" }),
    ));
    let banner = events
        .claim_session_banner_line(other_channel, &ProviderKind::Claude)
        .expect("fresh session yields a one-shot banner");
    assert!(!banner.contains("최근 대화"));
}

#[test]
fn status_panel_omits_session_line_when_lifecycle_details_are_absent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(180);
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        None,
        "session_fresh",
        &json!({
            "reason": "idle_timeout",
            "recoveryMessageCount": 25,
        }),
    ));
    // #3983 item4: the fresh session yields a one-shot banner (not a footer line).
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .expect("fresh session yields a one-shot banner")
            .contains("🆕 새 세션 시작")
    );

    // Empty lifecycle details drop the session snapshot, so there is nothing to
    // banner and nothing in the footer.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        None,
        "session_resumed",
        &json!({}),
    ));
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_none()
    );

    let footer = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(!footer.contains("Lifecycle "));
    assert!(!footer.contains("새 세션 시작"));
    assert!(!footer.contains("최근 대화"));
}

/// #3983 item4 — dual-path de-dup, SINK arrives first. The session-panel snapshot
/// is refreshed from both the bridge (sink) and the tmux watcher. Modelling the
/// sink reaching the atomic claim first: it wins the one-shot banner and the
/// watcher's subsequent claim (same session) observes the recorded key and skips.
#[test]
fn session_banner_claimed_exactly_once_sink_first() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(39831);
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch39831#100"),
        "session_fresh",
        &json!({ "provider_session_id": "session-A", "tmux_reused": false }),
    ));

    // Sink path claims first → gets the banner.
    let sink = events.claim_session_banner_line(channel_id, &ProviderKind::Claude);
    assert!(
        sink.as_deref()
            .is_some_and(|line| line.contains("🆕 새 세션 시작")),
        "the first (sink) claim yields the one-shot banner"
    );
    // Watcher path claims second for the SAME session → nothing.
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_none(),
        "the second (watcher) claim for the same session must not double-post"
    );
}

/// #3983 item4 — dual-path de-dup, WATCHER arrives first. Symmetric to the
/// sink-first case: whichever refresh path reaches the atomic claim first emits
/// the banner, proving the claim is order-independent (no double post, no
/// omission) when the two paths race.
#[test]
fn session_banner_claimed_exactly_once_watcher_first() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(39832);
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch39832#100"),
        "session_resumed",
        &json!({ "provider_session_id": "session-A", "tmux_reused": true }),
    ));

    // Watcher path claims first → gets the banner.
    let watcher = events.claim_session_banner_line(channel_id, &ProviderKind::Claude);
    assert!(
        watcher
            .as_deref()
            .is_some_and(|line| line.contains("기존 세션 복원")),
        "the first (watcher) claim yields the one-shot banner"
    );
    // Sink path claims second for the SAME session → nothing.
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_none(),
        "the second (sink) claim for the same session must not double-post"
    );
}

/// #3983 item4 — a genuine new-session boundary (new spawn nonce → new
/// `session_instance_key`) re-arms the claim, so the NEXT session gets its own
/// one-shot banner exactly once, while unrelated field churn within the same
/// session never re-posts.
#[test]
fn session_banner_reemits_once_on_new_session_boundary() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(39833);

    // Session A → one banner, then deduped.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch39833#100"),
        "session_fresh",
        &json!({ "provider_session_id": "session-A", "tmux_reused": false }),
    ));
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_some()
    );
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_none()
    );

    // Session B (new spawn nonce) → a fresh one-shot banner.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch39833#200"),
        "session_fresh",
        &json!({ "provider_session_id": "session-B", "tmux_reused": false }),
    ));
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_some(),
        "a new session INSTANCE re-arms the one-shot banner"
    );
    // Field churn within session B (same instance key + provider id) → no re-post.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch39833#200"),
        "session_fresh",
        &json!({ "provider_session_id": "session-B", "tmux_reused": true }),
    ));
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_none(),
        "unrelated field churn within the same session must not re-post the banner"
    );
}

/// #3983 item4 — with no live tmux marker (`session_instance_key == None`) the
/// dedup falls back to the provider session id, so a headless session still
/// banners exactly once and a genuinely different provider session re-arms it.
#[test]
fn session_banner_dedup_falls_back_to_provider_session_id() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(39834);

    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        None, // no instance key → provider-session-id fallback
        "session_resumed",
        &json!({ "provider_session_id": "prov-A", "tmux_reused": true }),
    ));
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_some()
    );
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_none()
    );

    // A genuinely different provider session (still no instance key) re-arms.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        None,
        "session_resumed",
        &json!({ "provider_session_id": "prov-B", "tmux_reused": true }),
    ));
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_some(),
        "a different provider session re-arms the one-shot banner"
    );
}

/// #3983 item4 — a channel with no session snapshot (or a cleared one) yields no
/// banner, so the emit path never posts a spurious top message.
#[test]
fn session_banner_none_without_session_snapshot() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(39835);

    // Never set → nothing to claim.
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_none()
    );

    // Set then clear → nothing to claim.
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch39835#100"),
        "session_fresh",
        &json!({ "provider_session_id": "session-A", "tmux_reused": false }),
    ));
    assert!(events.clear_session_panel(channel_id));
    assert!(
        events
            .claim_session_banner_line(channel_id, &ProviderKind::Claude)
            .is_none()
    );
}

#[test]
fn status_panel_omits_context_line_when_token_data_is_absent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(181);

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    assert!(!rendered.contains("Context   "));
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
fn status_panel_clamps_codex_context_usage_display_to_window() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(189);
    assert!(events.set_context_panel_usage(channel_id, None, 2_300_000, 0, 0, 272_000, 60));

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Codex, 1_700_000_000);

    assert!(rendered.contains("Context   ⚠️ 272.0k / 272.0k tokens (100%) · auto-compact 60%"));
    assert!(!rendered.contains("2.3M"));
}

#[test]
fn completion_footer_context_only_has_no_spinner_and_stops_scheduling() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(190);
    assert!(events.set_context_panel_usage(channel_id, None, 154_600, 0, 0, 1_000_000, 60));

    let rendered = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let block = rendered.block.expect("context line should render");

    assert!(block.contains("Context   📦 154.6k / 1.0M tokens (15%) · auto-compact 60%"));
    assert!(!block.contains('⠸'));
    assert!(!rendered.has_unfinished_entries);
}

#[test]
fn completion_footer_running_background_subagent_animates_until_notification_done() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(191);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "bgworker",
                "description": "Long background job",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_bg"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), false, Some("toolu_bg")),
    );

    let running = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let running_block = running.block.expect("running subagent should render");
    assert!(running.has_unfinished_entries);
    assert!(running_block.contains("Subagents"));
    assert!(running_block.contains("bgworker Long background job ⠸"));
    assert!(!running_block.contains('✓'));

    events.push_status_events(
        channel_id,
        status_events_from_task_notification("subagent", "completed", "all done"),
    );
    let done = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    let done_block = done.block.expect("finished subagent should stay visible");
    assert!(!done.has_unfinished_entries);
    assert!(done_block.contains("bgworker Long background job"));
    assert!(done_block.contains("all done"));
    assert!(done_block.contains('✓'));
    assert!(!done_block.contains('⠼'));
}

#[test]
fn background_bash_footer_mode_creates_running_task_slot_and_ack_stays_running() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_089_100);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "codex exec --skip-git-repo-check",
                "description": "Launch codex for SharedData S4",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_bash_bg"),
            true,
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Bash"), false, Some("toolu_bash_bg")),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let line = rendered
        .lines()
        .find(|line| line.contains("Bash Launch codex for SharedData S4"))
        .unwrap_or_else(|| panic!("background Bash task slot missing in: {rendered}"));

    assert!(rendered.contains("Tasks"));
    assert!(
        !line.contains('✓') && !line.contains('✗') && !line.contains('⠸'),
        "running background Bash slot must not show a terminal marker/spinner in live panel: {line}"
    );
}

#[test]
fn background_bash_notification_finalizes_only_exact_tool_use_id() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_089_101);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "codex exec",
                "description": "Launch codex for voice S2",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_voice"),
            true,
        ),
    );

    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "background",
            "completed",
            "Background command \"Launch codex for other\" completed (exit code 0)",
            Some("toolu_other"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "background",
            "completed",
            "Background command without id completed (exit code 0)",
            None,
        ),
    );

    let still_running =
        events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let running_line = still_running
        .lines()
        .find(|line| line.contains("Bash Launch codex for voice S2"))
        .unwrap_or_else(|| panic!("background Bash task slot missing in: {still_running}"));
    assert!(
        !running_line.contains('✓') && !running_line.contains('✗'),
        "non-matching or id-less notification must not finalize: {running_line}"
    );

    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "background",
            "completed",
            "Background command \"Launch codex for voice S2\" completed (exit code 0)",
            Some("toolu_voice"),
        ),
    );

    let done = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let done_line = done
        .lines()
        .find(|line| line.contains("Bash Launch codex for voice S2"))
        .unwrap_or_else(|| panic!("background Bash task slot missing in: {done}"));
    assert!(
        done_line.contains('✓'),
        "matching notification must mark ✓: {done_line}"
    );
}

#[test]
fn background_bash_failed_notification_marks_task_failed() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_089_102);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "deploy-release.sh",
                "description": "Deploy release runtime",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_deploy"),
            true,
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "background",
            "failed",
            "Background command \"Deploy release runtime\" failed with exit code 1",
            Some("toolu_deploy"),
        ),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let line = rendered
        .lines()
        .find(|line| line.contains("Bash Deploy release runtime"))
        .unwrap_or_else(|| panic!("background Bash task slot missing in: {rendered}"));
    assert!(
        line.contains('✗'),
        "failed notification must mark ✗: {line}"
    );
}

#[test]
fn background_bash_record_reconstruction_ack_does_not_finalize_then_notification_flips() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_089_103);
    events.push_status_events(
        channel_id,
        status_events_from_json_for_footer_mode(
            &json!({
                "type": "assistant",
                "message": {
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_record_bash",
                        "name": "Bash",
                        "input": {
                            "command": "cd /tmp/adk && codex exec",
                            "description": "Launch codex for SharedData S4",
                            "run_in_background": true
                        }
                    }]
                }
            }),
            true,
        ),
    );

    let ack_events = status_events_from_json_for_footer_mode(
        &json!({
            "type": "user",
            "message": {
                "content": [{
                    "tool_use_id": "toolu_record_bash",
                    "type": "tool_result",
                    "content": "Command running in background with ID: bdri3xti5. Output is being written to: /tmp/tasks/bdri3xti5.output.",
                    "is_error": false
                }]
            },
            "toolUseResult": {
                "stdout": "",
                "stderr": "",
                "interrupted": false,
                "isImage": false,
                "noOutputExpected": false,
                "backgroundTaskId": "bdri3xti5"
            }
        }),
        true,
    );
    assert!(
        !ack_events
            .iter()
            .any(|event| matches!(event, StatusEvent::BackgroundTaskEnd { .. })),
        "background Bash launch ACK must not synthesize BackgroundTaskEnd: {ack_events:?}"
    );
    events.push_status_events(channel_id, ack_events);

    let running = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let running_line = running
        .lines()
        .find(|line| line.contains("Bash Launch codex for SharedData S4"))
        .unwrap_or_else(|| panic!("background Bash task slot missing in: {running}"));
    assert!(
        !running_line.contains('✓') && !running_line.contains('✗'),
        "launch ACK must leave reconstructed slot running: {running_line}"
    );

    events.push_status_events(
        channel_id,
        status_events_from_json_for_footer_mode(
            &json!({
                "type": "system",
                "subtype": "task_notification",
                "task_notification_kind": "background",
                "task_id": "bdri3xti5",
                "tool_use_id": "toolu_record_bash",
                "status": "completed",
                "summary": "Background command \"Launch codex for SharedData S4\" completed (exit code 0)"
            }),
            true,
        ),
    );

    let done = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let done_line = done
        .lines()
        .find(|line| line.contains("Bash Launch codex for SharedData S4"))
        .unwrap_or_else(|| panic!("background Bash task slot missing in: {done}"));
    assert!(
        done_line.contains('✓'),
        "matching reconstructed notification must flip ✓: {done_line}"
    );
}

#[test]
fn completion_footer_background_bash_animates_and_flips_on_notification() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_089_104);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "codex exec --skip-git-repo-check",
                "description": "Delegate background task slots to codex",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_delegate"),
            true,
        ),
    );

    let running = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let running_block = running
        .block
        .expect("running background Bash should render");
    assert!(running.has_unfinished_entries);
    assert!(running_block.contains("Tasks"));
    assert!(running_block.contains("Bash Delegate background task slots to codex ⠸"));
    assert!(!running_block.contains('✓'));

    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "background",
            "completed",
            "Background command \"Delegate background task slots to codex\" completed (exit code 0)",
            Some("toolu_delegate"),
        ),
    );
    let done = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    let done_block = done
        .block
        .expect("finished background Bash should stay visible");
    assert!(!done.has_unfinished_entries);
    assert!(done_block.contains("Bash Delegate background task slots to codex ✓"));
    assert!(!done_block.contains('⠼'));
}

fn push_background_bash_task(
    events: &PlaceholderLiveEvents,
    channel_id: ChannelId,
    summary: &str,
    tool_use_id: &str,
) {
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "sleep 1",
                "description": summary,
                "run_in_background": true
            })
            .to_string(),
            Some(tool_use_id),
            true,
        ),
    );
}

// #3391: a background task's slot identity keys on its `tool_use_id`.
fn bg_task_id(tool_use_id: &str) -> super::completion_footer::TerminalSlotId {
    super::completion_footer::TerminalSlotId::Task(super::completion_footer::SlotKey::ToolUseId(
        tool_use_id.to_string(),
    ))
}

// #3391: a subagent slot keyed on its launching `tool_use_id`.
fn subagent_id(tool_use_id: &str) -> super::completion_footer::TerminalSlotId {
    super::completion_footer::TerminalSlotId::Subagent(
        super::completion_footer::SlotKey::ToolUseId(tool_use_id.to_string()),
    )
}

fn complete_background_bash_task(
    events: &PlaceholderLiveEvents,
    channel_id: ChannelId,
    tool_use_id: &str,
) {
    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "background",
            "completed",
            "Background command completed (exit code 0)",
            Some(tool_use_id),
        ),
    );
}

#[test]
fn completion_footer_delivered_terminal_task_evicts_from_next_render() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_391_001);
    push_background_bash_task(&events, channel_id, "Keep running", "toolu_3391_run");
    push_background_bash_task(&events, channel_id, "Evict after ack", "toolu_3391_done");
    complete_background_bash_task(&events, channel_id, "toolu_3391_done");

    let delivered = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let block = delivered
        .block
        .expect("running + finished tasks should render");
    assert!(block.contains("Bash Evict after ack ✓"));
    assert!(block.contains("Bash Keep running ⠸"));
    assert_eq!(
        delivered.delivered_terminal_ids,
        vec![bg_task_id("toolu_3391_done")]
    );

    events.evict_delivered_terminal_footer_tasks(channel_id, &delivered.delivered_terminal_ids);

    let next = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    let next_block = next.block.expect("running task should keep rendering");
    assert!(!next_block.contains("Evict after ack"));
    assert!(next_block.contains("Bash Keep running ⠼"));
    assert!(next.has_unfinished_entries);
    assert!(next.delivered_terminal_ids.is_empty());
}

#[test]
fn completion_footer_undelivered_terminal_task_keeps_rendering_and_inflight_never_evicts() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_391_002);
    push_background_bash_task(&events, channel_id, "Stay running", "toolu_3391_stay");
    push_background_bash_task(
        &events,
        channel_id,
        "Retry my checkmark",
        "toolu_3391_retry",
    );
    complete_background_bash_task(&events, channel_id, "toolu_3391_retry");

    // A failed Discord edit never acks the render, so the ✓ renders again.
    let first = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    assert_eq!(first.delivered_terminal_ids.len(), 1);
    let retry = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    assert!(
        retry
            .block
            .expect("undelivered terminal task should re-render")
            .contains("Bash Retry my checkmark ✓")
    );
    assert_eq!(retry.delivered_terminal_ids, first.delivered_terminal_ids);

    // Stale/unknown identities and the in-flight slot's id never evict anything.
    events.evict_delivered_terminal_footer_tasks(
        channel_id,
        &[bg_task_id("toolu_3391_stay"), bg_task_id("toolu_unknown")],
    );
    let after = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let after_block = after.block.expect("both tasks should still render");
    assert!(after_block.contains("Bash Retry my checkmark ✓"));
    assert!(after_block.contains("Bash Stay running ⠸"));
}

#[test]
fn completion_footer_evicts_all_terminal_tasks_delivered_in_one_render() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_391_003);
    push_background_bash_task(&events, channel_id, "First done", "toolu_3391_a");
    push_background_bash_task(&events, channel_id, "Second done", "toolu_3391_b");
    push_background_bash_task(&events, channel_id, "Still running", "toolu_3391_c");
    complete_background_bash_task(&events, channel_id, "toolu_3391_a");
    complete_background_bash_task(&events, channel_id, "toolu_3391_b");

    let delivered = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    assert_eq!(delivered.delivered_terminal_ids.len(), 2);

    events.evict_delivered_terminal_footer_tasks(channel_id, &delivered.delivered_terminal_ids);

    let next = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    let next_block = next.block.expect("running task should keep rendering");
    assert!(!next_block.contains("First done"));
    assert!(!next_block.contains("Second done"));
    assert!(next_block.contains("Bash Still running ⠼"));
    assert!(next.has_unfinished_entries);
}

#[test]
fn completion_footer_terminal_lines_clamped_out_of_budget_are_not_delivered() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_391_004);
    for i in 0..STATUS_PANEL_TASK_LIMIT {
        let tool_use_id = format!("toolu_3391_clamp_{i:02}");
        push_background_bash_task(
            &events,
            channel_id,
            &format!("Clamp slot {i:02} {}", "x".repeat(70)),
            &tool_use_id,
        );
        complete_background_bash_task(&events, channel_id, &tool_use_id);
    }

    let first = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let _first_block = first.block.expect("clamped task section should render");
    let first_delivered = first.delivered_terminal_ids.clone();
    assert!(
        !first_delivered.is_empty() && first_delivered.len() < STATUS_PANEL_TASK_LIMIT,
        "the 600B clamp should deliver some but not all terminal slots: {}",
        first_delivered.len()
    );

    events.evict_delivered_terminal_footer_tasks(channel_id, &first_delivered);

    // The clamped-out (undelivered) slots are still terminal and re-render with
    // their marks; the delivered ones are gone, so their identities cannot
    // reappear in a later render's delivered set.
    let second = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    second
        .block
        .expect("clamped-out terminal tasks must render on a later pass");
    for id in &first_delivered {
        assert!(
            !second.delivered_terminal_ids.contains(id),
            "an already-evicted slot id must not re-deliver: {id:?}"
        );
    }
    assert!(!second.delivered_terminal_ids.is_empty());
}

// #3391 Finding 1(a) collision pin: two slots render the IDENTICAL terminal
// line but only ONE survives the 600B clamp. Slot-identity eviction must drop
// EXACTLY the delivered slot; the clamped-out duplicate keeps its ✓ and
// re-renders. The old line-string eviction dropped BOTH (matched the shared
// line), permanently swallowing the clamped-out mark, so this FAILS on HEAD.
#[test]
fn completion_footer_identical_terminal_lines_evict_only_the_delivered_slot() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_391_201);
    const DUP_SUMMARY: &str = "Wait until CI settles";

    // Push dup_a first, then padding, then dup_b. Newest renders first (`.rev()`),
    // so render order is [Tasks, dup_b, ...padding, dup_a]: dup_b survives the
    // clamp while dup_a is pushed past the 600B budget.
    push_background_bash_task(&events, channel_id, DUP_SUMMARY, "toolu_dup_a");
    complete_background_bash_task(&events, channel_id, "toolu_dup_a");
    for i in 0..8 {
        let id = format!("toolu_pad_{i:02}");
        push_background_bash_task(
            &events,
            channel_id,
            &format!("Padding job {i:02} {}", "y".repeat(80)),
            &id,
        );
        complete_background_bash_task(&events, channel_id, &id);
    }
    push_background_bash_task(&events, channel_id, DUP_SUMMARY, "toolu_dup_b");
    complete_background_bash_task(&events, channel_id, "toolu_dup_b");

    let first = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let first_block = first.block.expect("clamped task section should render");
    let dup_line = format!("└ Bash {DUP_SUMMARY} ✓");
    assert!(
        first_block.contains(&dup_line),
        "the surviving duplicate's line should render: {first_block}"
    );
    // dup_b survives the clamp and is delivered; dup_a was clamped out.
    assert!(
        first
            .delivered_terminal_ids
            .contains(&bg_task_id("toolu_dup_b"))
    );
    assert!(
        !first
            .delivered_terminal_ids
            .contains(&bg_task_id("toolu_dup_a")),
        "the clamped-out duplicate must NOT be reported delivered"
    );

    events.evict_delivered_terminal_footer_tasks(channel_id, &first.delivered_terminal_ids);

    // After eviction, dup_a (never delivered) is still terminal and re-renders
    // its identical ✓ line. With the old line-string eviction both vanished.
    let second = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    let second_block = second.block.expect("clamped-out duplicate must re-render");
    assert!(
        second_block.contains(&dup_line),
        "the clamped-out duplicate's ✓ must survive eviction of its twin: {second_block}"
    );
    assert!(
        second
            .delivered_terminal_ids
            .contains(&bg_task_id("toolu_dup_a"))
    );
    assert!(
        !second
            .delivered_terminal_ids
            .contains(&bg_task_id("toolu_dup_b")),
        "the already-evicted slot must not re-appear"
    );
}

// #3391 Finding 1(b) race pin: a running slot turns terminal AFTER its render
// snapshot but BEFORE ack, and the delivered mark belonged to a DIFFERENT slot
// whose line is identical. The newly-terminal slot must NOT be evicted — its
// own mark was never shown. The old line-string eviction matched the shared
// line and dropped it, so this FAILS on HEAD.
#[test]
fn completion_footer_slot_turning_terminal_before_ack_is_not_evicted_on_twin_line() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_391_202);
    const TWIN_SUMMARY: &str = "Bash Wait until CI settles";

    // delivered: completed twin whose mark IS in this render.
    push_background_bash_task(&events, channel_id, TWIN_SUMMARY, "toolu_delivered");
    complete_background_bash_task(&events, channel_id, "toolu_delivered");
    // racing: identical summary, still RUNNING at render time.
    push_background_bash_task(&events, channel_id, TWIN_SUMMARY, "toolu_racing");

    let delivered = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    // Only the completed twin's id is delivered; the running slot is in-flight.
    assert_eq!(
        delivered.delivered_terminal_ids,
        vec![bg_task_id("toolu_delivered")]
    );

    // The edit is in flight; before the ack lands the racing slot completes and
    // now renders the IDENTICAL terminal line as the delivered twin.
    complete_background_bash_task(&events, channel_id, "toolu_racing");

    events.evict_delivered_terminal_footer_tasks(channel_id, &delivered.delivered_terminal_ids);

    // The racing slot's ✓ was never delivered, so it must still render and be
    // reportable on the next pass; only the delivered twin is gone.
    let next = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    next.block.expect("racing slot's mark must still render");
    assert_eq!(
        next.delivered_terminal_ids,
        vec![bg_task_id("toolu_racing")],
        "only the racing slot's never-delivered mark should remain"
    );
}

// #3391 Finding 2: terminal SUBAGENT slots must evict on confirmed delivery,
// in-flight subagents are untouched, and the migration carry-over filters
// evicted subagents. On HEAD eviction only retained over `tasks`, so subagents
// accumulated and this FAILS for the eviction part.
#[test]
fn completion_footer_terminal_subagent_evicts_after_delivery_inflight_unaffected() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_391_203);
    events.push_status_event(
        channel_id,
        StatusEvent::SubagentStart {
            subagent_type: Some("reviewer".to_string()),
            desc: Some("Audit the diff".to_string()),
            tool_use_id: Some("toolu_done_sub".to_string()),
            background: false,
        },
    );
    events.push_status_event(
        channel_id,
        StatusEvent::SubagentStart {
            subagent_type: Some("reviewer".to_string()),
            desc: Some("Still inspecting".to_string()),
            tool_use_id: Some("toolu_running_sub".to_string()),
            background: false,
        },
    );
    events.push_status_event(
        channel_id,
        StatusEvent::SubagentEnd {
            success: true,
            tool_use_id: Some("toolu_done_sub".to_string()),
            summary: None,
            ack_only: false,
        },
    );

    let delivered = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let block = delivered.block.expect("subagents should render");
    assert!(block.contains("Audit the diff ✓"));
    assert!(block.contains("Still inspecting ⠸"));
    assert_eq!(
        delivered.delivered_terminal_ids,
        vec![subagent_id("toolu_done_sub")]
    );

    events.evict_delivered_terminal_footer_tasks(channel_id, &delivered.delivered_terminal_ids);

    let next = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    let next_block = next.block.expect("running subagent keeps rendering");
    assert!(
        !next_block.contains("Audit the diff"),
        "delivered terminal subagent must be evicted: {next_block}"
    );
    assert!(next_block.contains("Still inspecting ⠼"));
    assert!(next.has_unfinished_entries);
    assert!(next.delivered_terminal_ids.is_empty());
}

// #3391 round 3 helper: pull the single rendered footer line that contains
// `needle` so a test can assert what its TAIL looks like after truncation.
fn footer_line_containing<'a>(block: &'a str, needle: &str) -> &'a str {
    block
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no footer line contains {needle:?}: {block}"))
}

// #3391 round 3 review P2: degenerate budgets must never exceed `max_chars`.
// `truncate_chars` emits up to 3 chars ("...") below its budget, so budgets
// under marker_reserve+3 degrade to a hard clamp (marker may be lost there —
// the delivered-ID honesty gate then keeps the slot un-evicted).
#[test]
fn truncate_chars_with_marker_never_exceeds_max_chars_on_degenerate_budgets() {
    for max_chars in [0usize, 1, 2, 3, 4, 5] {
        let line = super::common::truncate_chars_with_marker("a long base", "✓", max_chars);
        assert!(
            line.chars().count() <= max_chars,
            "budget {max_chars}: {line:?} exceeds the contract"
        );
    }
    // Sound budgets keep the marker guarantee.
    let line = super::common::truncate_chars_with_marker(&"x".repeat(200), "✓", 100);
    assert!(
        line.ends_with('✓') && line.chars().count() <= 100,
        "{line:?}"
    );
}

// #3391 round 3 finding 1 (task): a background task whose description is long
// enough that the pre-fix append-then-truncate swallowed the mark must still
// render a line that ENDS WITH ✓. FAILS on HEAD 95f6e2176 (the ✓ was chopped
// off the >EVENT_LINE_MAX_CHARS line).
#[test]
fn completion_footer_long_background_task_line_ends_with_check_mark() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_391_301);
    let long_desc = format!("Long bg task {}", "x".repeat(EVENT_LINE_MAX_CHARS));
    push_background_bash_task(&events, channel_id, &long_desc, "toolu_3391_long_task");
    complete_background_bash_task(&events, channel_id, "toolu_3391_long_task");

    let rendered = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let block = rendered.block.expect("long terminal task should render");
    let line = footer_line_containing(&block, "Long bg task");
    assert!(
        line.chars().count() > EVENT_LINE_MAX_CHARS - 2,
        "test must exercise the truncation path: {line:?}"
    );
    assert!(
        line.ends_with('✓'),
        "long terminal background task line must end with ✓: {line:?}"
    );
    assert!(line.chars().count() <= EVENT_LINE_MAX_CHARS);
}

// #3391 round 3 finding 1 (subagent): same shape for a finished subagent slot
// with a long desc — the rendered line must END WITH ✓. FAILS on HEAD.
#[test]
fn completion_footer_long_subagent_line_ends_with_check_mark() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_391_302);
    let long_desc = format!("Audit subagent {}", "y".repeat(EVENT_LINE_MAX_CHARS));
    events.push_status_event(
        channel_id,
        StatusEvent::SubagentStart {
            subagent_type: Some("reviewer".to_string()),
            desc: Some(long_desc),
            tool_use_id: Some("toolu_3391_long_sub".to_string()),
            background: false,
        },
    );
    events.push_status_event(
        channel_id,
        StatusEvent::SubagentEnd {
            success: true,
            tool_use_id: Some("toolu_3391_long_sub".to_string()),
            summary: None,
            ack_only: false,
        },
    );

    let rendered = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let block = rendered
        .block
        .expect("long terminal subagent should render");
    let line = footer_line_containing(&block, "Audit subagent");
    assert!(
        line.chars().count() > EVENT_LINE_MAX_CHARS - 2,
        "test must exercise the truncation path: {line:?}"
    );
    assert!(
        line.ends_with('✓'),
        "long terminal subagent line must end with ✓: {line:?}"
    );
    assert!(line.chars().count() <= EVENT_LINE_MAX_CHARS);
}

// #3391 round 3 finding 2/3 (honesty): a terminal slot whose mark would (pre-fix)
// be truncated off its line must, post-fix, show the mark AND be reported in the
// delivered set — the two are pinned together. On HEAD the ✓ is chopped, so the
// `ends_with('✓')` assertion FAILS; post-fix the mark survives and the id ships.
#[test]
fn completion_footer_long_task_visible_mark_and_id_reported_together() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_391_303);
    let long_desc = format!("Honesty task {}", "z".repeat(EVENT_LINE_MAX_CHARS));
    push_background_bash_task(&events, channel_id, &long_desc, "toolu_3391_honesty");
    complete_background_bash_task(&events, channel_id, "toolu_3391_honesty");

    let rendered = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let block = rendered.block.expect("long terminal task should render");
    let line = footer_line_containing(&block, "Honesty task");
    let mark_visible = line.ends_with('✓');
    let id_reported = rendered
        .delivered_terminal_ids
        .contains(&bg_task_id("toolu_3391_honesty"));
    // Mark visibility and delivered-id reporting must agree: the honesty gate
    // never reports an id whose mark the user cannot see, and fix 1 keeps the
    // mark visible, so both are true together.
    assert!(
        mark_visible,
        "post-fix the ✓ must survive truncation: {line:?}"
    );
    assert!(
        id_reported,
        "a visible terminal mark must be reported as delivered: {:?}",
        rendered.delivered_terminal_ids
    );
    assert_eq!(
        mark_visible, id_reported,
        "mark visibility and delivered-id reporting must agree"
    );
}

// #3391 Finding 2 migration filter: the #3386 carry-over (clear-preserving
// residuals) must drop an EVICTED terminal subagent. A background subagent that
// completed and was evicted on delivery must not re-appear in the carried
// footer; an in-flight background subagent does carry over.
#[test]
fn completion_footer_evicted_subagent_does_not_survive_migration_carry_over() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_391_204);
    events.push_status_event(
        channel_id,
        StatusEvent::SubagentStart {
            subagent_type: Some("bgworker".to_string()),
            desc: Some("Finished bg agent".to_string()),
            tool_use_id: Some("toolu_bg_done".to_string()),
            background: true,
        },
    );
    events.push_status_event(
        channel_id,
        StatusEvent::SubagentStart {
            subagent_type: Some("bgworker".to_string()),
            desc: Some("Running bg agent".to_string()),
            tool_use_id: Some("toolu_bg_run".to_string()),
            background: true,
        },
    );
    events.push_status_event(
        channel_id,
        StatusEvent::SubagentEnd {
            success: true,
            tool_use_id: Some("toolu_bg_done".to_string()),
            summary: None,
            ack_only: false,
        },
    );

    let delivered = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    assert_eq!(
        delivered.delivered_terminal_ids,
        vec![subagent_id("toolu_bg_done")]
    );
    events.evict_delivered_terminal_footer_tasks(channel_id, &delivered.delivered_terminal_ids);

    // #3386 migration carry-over: only unfinished background residuals survive.
    events.clear_channel_preserving_footer_residuals(channel_id);
    let carried = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    let carried_block = carried
        .block
        .expect("running residual subagent carries over");
    assert!(
        !carried_block.contains("Finished bg agent"),
        "an evicted terminal subagent must not carry over: {carried_block}"
    );
    assert!(carried_block.contains("Running bg agent"));
}

#[test]
fn footer_residual_entries_carry_to_next_turn_and_finished_entries_do_not() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_089_107);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "codex exec carry",
                "description": "Carry bash task",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_carry_bash"),
            true,
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "codex exec done",
                "description": "Finished bash task",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_done_bash"),
            true,
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "background",
            "completed",
            "Background command completed",
            Some("toolu_done_bash"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "bgworker",
                "description": "Carry agent task",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_carry_agent"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), false, Some("toolu_carry_agent")),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "bgworker",
                "description": "Finished agent task",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_done_agent"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), false, Some("toolu_done_agent")),
    );
    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "subagent",
            "completed",
            "finished agent done",
            Some("toolu_done_agent"),
        ),
    );

    events.clear_channel_preserving_footer_residuals(channel_id);

    let live = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(live.contains("Bash Carry bash task"));
    assert!(live.contains("bgworker Carry agent task"));
    assert!(!live.contains("Finished bash task"));
    assert!(!live.contains("Finished agent task"));

    let footer = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let footer_block = footer.block.expect("carried residual footer should render");
    assert!(footer.has_unfinished_entries);
    assert!(footer_block.contains("Bash Carry bash task ⠸"));
    assert!(footer_block.contains("bgworker Carry agent task ⠸"));
    assert!(!footer_block.contains("Finished bash task"));
    assert!(!footer_block.contains("Finished agent task"));

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "codex exec carry replay",
                "description": "Carry bash task replay",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_carry_bash"),
            true,
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "bgworker",
                "description": "Carry agent task replay",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_carry_agent"),
        ),
    );

    let deduped = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let slot_lines = deduped
        .lines()
        .filter(|line| line.starts_with("└ "))
        .collect::<Vec<_>>();
    assert_eq!(deduped.matches("toolu_carry_bash").count(), 0);
    assert_eq!(
        slot_lines
            .iter()
            .filter(|line| line.contains("Carry bash task"))
            .count(),
        1
    );
    assert_eq!(
        slot_lines
            .iter()
            .filter(|line| line.contains("Carry agent task"))
            .count(),
        1
    );
}

#[test]
fn carried_residual_entries_finalize_by_exact_tool_use_id_on_latest_state() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_089_108);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "codex exec exact",
                "description": "Exact carried bash",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_exact_bash"),
            true,
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "bgworker",
                "description": "Exact carried agent",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_exact_agent"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), false, Some("toolu_exact_agent")),
    );
    events.clear_channel_preserving_footer_residuals(channel_id);

    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "background",
            "completed",
            "wrong bash complete",
            Some("toolu_other_bash"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "subagent",
            "completed",
            "wrong agent complete",
            Some("toolu_other_agent"),
        ),
    );
    let still_running = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let still_running_block = still_running.block.expect("carried entries should render");
    assert!(still_running.has_unfinished_entries);
    assert!(still_running_block.contains("Exact carried bash ⠸"));
    assert!(still_running_block.contains("Exact carried agent ⠸"));
    assert!(!still_running_block.contains('✓'));

    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "background",
            "completed",
            "exact bash complete",
            Some("toolu_exact_bash"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "subagent",
            "completed",
            "exact agent complete",
            Some("toolu_exact_agent"),
        ),
    );
    let done = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    let done_block = done.block.expect("finished carried entries should render");
    assert!(!done.has_unfinished_entries);
    assert!(done_block.contains("Exact carried bash ✓"));
    assert!(done_block.contains("Exact carried agent — exact agent complete ✓"));
    assert!(!done_block.contains('⠼'));
}

#[test]
fn background_bash_slots_are_footer_flag_gated() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_089_105);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "codex exec",
                "description": "Should stay hidden with footer flag off",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_hidden"),
            false,
        ),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(!rendered.contains("Tasks"));
    assert!(!rendered.contains("└ Bash Should stay hidden with footer flag off"));
}

#[test]
fn background_bash_command_only_slot_hides_raw_command_3806() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_806_002);
    let raw_command = "codex exec --skip-git-repo-check -m gpt-5.5";
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": raw_command,
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_raw_command_hidden"),
            true,
        ),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("Tasks"));
    assert!(
        rendered.contains("└ Bash"),
        "background Bash class should remain visible: {rendered}"
    );
    assert!(
        !rendered.contains(raw_command),
        "background Bash slot must not leak raw command detail: {rendered}"
    );
}

#[test]
fn background_bash_task_slots_trim_to_task_limit() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_089_106);
    for idx in 0..=STATUS_PANEL_TASK_LIMIT {
        let tool_use_id = format!("toolu_bg_{idx}");
        events.push_status_events(
            channel_id,
            status_events_from_tool_use_with_id_for_footer_mode(
                "Bash",
                &json!({
                    "command": format!("codex exec {idx}"),
                    "description": format!("background bash task {idx}"),
                    "run_in_background": true
                })
                .to_string(),
                Some(&tool_use_id),
                true,
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
        guard.tasks.first().and_then(|slot| slot.summary.as_deref()),
        Some("background bash task 1")
    );
    assert_eq!(
        guard.tasks.last().and_then(|slot| slot.summary.as_deref()),
        Some("background bash task 10")
    );
    assert!(!rendered.contains("background bash task 0"));
    assert!(rendered.contains("background bash task 1"));
    assert!(rendered.contains("background bash task 10"));
}

#[test]
fn completion_footer_budget_clamps_task_section_but_keeps_context_line() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(192);
    assert!(events.set_context_panel_usage(channel_id, None, 154_600, 0, 0, 1_000_000, 60));
    for idx in 0..40 {
        let tool_id = format!("toolu_{idx}");
        events.push_status_events(
            channel_id,
            status_events_from_tool_use_with_id(
                "Task",
                &json!({
                    "subagent_type": "reviewer",
                    "description": format!("Inspect very long completion footer task section {idx} {}", "x".repeat(80)),
                    "run_in_background": true
                })
                .to_string(),
                Some(&tool_id),
            ),
        );
    }

    let rendered = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let block = rendered.block.expect("completion footer should render");
    let task_section = block
        .split_once("\n\n")
        .map(|(_, task_section)| task_section)
        .expect("context and task sections should be separated");

    assert!(block.contains("Context   📦 154.6k / 1.0M tokens (15%) · auto-compact 60%"));
    assert!(task_section.len() <= crate::services::discord::single_message_panel::SINGLE_MESSAGE_PANEL_FOOTER_BUDGET_BYTES);
    assert!(task_section.ends_with('…'));
    assert!(rendered.has_unfinished_entries);
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

// #3084: a long-running Task subagent returns its result AFTER intervening
// short foreground tools. With FIFO pairing the wrong tool was popped and the
// real subagent's SubagentEnd never fired, leaving a ghost "running" marker
// (no ✓/✗). With tool_use_id pairing the delayed result must still close the
// correct slot as done.
#[test]
fn status_panel_pairs_subagent_by_tool_use_id_despite_interleaving() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(384);

    // Task A starts (long-running), id "task-a".
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Investigate #3084"}).to_string(),
            Some("task-a"),
        ),
    );
    // Foreground Bash use + result resolves first (id "bash-1").
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Bash",
            &json!({"command": "cargo test"}).to_string(),
            Some("bash-1"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Bash"), false, Some("bash-1")),
    );
    // Foreground Read use + result resolves next (id "read-1").
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Read",
            &json!({"file_path": "/tmp/x"}).to_string(),
            Some("read-1"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Read"), false, Some("read-1")),
    );
    // Finally Task A's own delayed result arrives, paired by id "task-a".
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), false, Some("task-a")),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("Subagents"));
    assert!(rendered.contains("explorer Investigate #3084"));
    // The subagent must be marked done — no ghost running marker.
    assert!(
        rendered.contains('✓'),
        "subagent should be closed as done, got: {rendered}"
    );
    assert!(
        !rendered.contains('✗'),
        "successful subagent must not show failure marker, got: {rendered}"
    );
}

// Live subagent activity: a nested subagent record carries the launching Task's
// id as a top-level `parent_tool_use_id`. Its tool class must surface on the
// owning subagent slot (`└ type desc — [Tool]`), not the panel header, so a long
// background subagent is not opaque while raw tool args stay out of the panel.
#[test]
fn status_panel_shows_live_subagent_activity_by_parent_id() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(900);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "general-purpose", "description": "Audit logs"}).to_string(),
            Some("task-z"),
        ),
    );
    // Nested subagent tool_use record (parent = the Task's id).
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "assistant",
            "parent_tool_use_id": "task-z",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "name": "Bash",
                    "input": {"command": "grep ERROR app.log"}
                }]
            }
        })),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("general-purpose Audit logs"));
    assert!(
        rendered.contains("[Bash]"),
        "subagent activity line missing, got: {rendered}"
    );
    assert!(
        !rendered.contains("grep ERROR app.log"),
        "subagent activity must not leak raw command args, got: {rendered}"
    );
    // Nested activity must NOT turn the panel header into a foreground tool run.
    assert!(
        !rendered.contains("🔧 도구 실행 중"),
        "nested subagent step must not clobber the panel header, got: {rendered}"
    );
}

// The activity line updates to the subagent's LATEST step on each new event.
#[test]
fn status_panel_subagent_activity_updates_to_latest_step() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(901);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Trace"}).to_string(),
            Some("t1"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "assistant",
            "parent_tool_use_id": "t1",
            "message": {"content": [{"type": "tool_use", "name": "Read", "input": {"file_path": "/a"}}]}
        })),
    );
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "assistant",
            "parent_tool_use_id": "t1",
            "message": {"content": [{"type": "tool_use", "name": "Grep", "input": {"pattern": "needle"}}]}
        })),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(
        rendered.contains("[Grep]"),
        "latest step missing, got: {rendered}"
    );
    assert!(
        !rendered.contains("[Read]"),
        "stale step retained, got: {rendered}"
    );
}

// #3198 safety: activity whose parent id matches a FINISHED slot must not
// resurrect it (no re-mark, no recent line on a closed background subagent), and
// an id-bearing activity that matches no slot is dropped, never mis-routed to an
// unrelated running subagent.
#[test]
fn status_panel_subagent_activity_never_resurrects_or_misroutes() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(902);

    // Slot A finished (foreground, closed by its ack).
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "alpha", "description": "Done work"}).to_string(),
            Some("a"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), false, Some("a")),
    );
    // Slot B still running.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "beta", "description": "Live work"}).to_string(),
            Some("b"),
        ),
    );

    // Late activity for the FINISHED slot A — must be ignored, not resurrected.
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "assistant",
            "parent_tool_use_id": "a",
            "message": {"content": [{"type": "tool_use", "name": "Bash", "input": {"command": "late-ghost"}}]}
        })),
    );
    // Activity for an UNKNOWN id — must be dropped, not routed to slot B.
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "assistant",
            "parent_tool_use_id": "ghost",
            "message": {"content": [{"type": "tool_use", "name": "Bash", "input": {"command": "stray"}}]}
        })),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(
        !rendered.contains("late-ghost"),
        "finished slot must not be resurrected, got: {rendered}"
    );
    assert!(
        !rendered.contains("stray"),
        "unmatched activity must not mis-route onto running slot B, got: {rendered}"
    );
}

// A background subagent keeps running past its launch ack (#3198), and its live
// activity surfaces on the still-open slot — exactly the visibility this feature
// adds.
#[test]
fn status_panel_background_subagent_shows_live_activity_while_running() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(903);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "general-purpose",
                "description": "Long background job",
                "run_in_background": true
            })
            .to_string(),
            Some("bg-1"),
        ),
    );
    // Launch ack — background slot stays open (not ✓).
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), false, Some("bg-1")),
    );
    // Live step from the still-running background subagent.
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "content_block_start",
            "parent_tool_use_id": "bg-1",
            "content_block": {"type": "tool_use", "name": "WebSearch", "input": {"query": "rust async"}}
        })),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(rendered.contains("general-purpose Long background job"));
    assert!(
        rendered.contains("[WebSearch]"),
        "background subagent live activity missing, got: {rendered}"
    );
    assert!(
        !rendered.contains("rust async"),
        "background subagent activity must not leak raw query args, got: {rendered}"
    );
    // Still running — no completion marker yet.
    assert!(
        !rendered.contains('✓'),
        "background subagent must not show ✓ on a launch ack, got: {rendered}"
    );
}

// #3084: two parallel subagents whose results return in reverse order must each
// close their own slot. The previous "first unfinished slot" logic closed the
// wrong slot, mis-attributing success/failure across parallel subagents.
#[test]
fn status_panel_parallel_subagents_close_correct_slots_in_reverse_order() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(385);

    // Task A and Task B both start.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "alpha", "description": "Task A work"}).to_string(),
            Some("task-a"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "beta", "description": "Task B work"}).to_string(),
            Some("task-b"),
        ),
    );
    // B finishes first (success), then A finishes (failure) — reverse order.
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), false, Some("task-b")),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), true, Some("task-a")),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    // Each slot rendered on its own line; assert per-slot markers so we catch
    // mis-attribution (e.g. A's failure landing on B).
    let alpha_line = rendered
        .lines()
        .find(|line| line.contains("alpha Task A work"))
        .unwrap_or_else(|| panic!("alpha slot missing in: {rendered}"));
    let beta_line = rendered
        .lines()
        .find(|line| line.contains("beta Task B work"))
        .unwrap_or_else(|| panic!("beta slot missing in: {rendered}"));
    assert!(
        alpha_line.contains('✗'),
        "Task A failed and must show ✗, got: {alpha_line}"
    );
    assert!(
        beta_line.contains('✓'),
        "Task B succeeded and must show ✓, got: {beta_line}"
    );
}

// #3086: a finished subagent whose `tool_result` record carries `toolUseResult`
// accounting renders the TUI-parity `Done (N tools · M tokens · Xs)` summary on
// the correct slot, paired by tool_use_id.
#[test]
fn status_panel_renders_subagent_done_summary_from_tool_use_result() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(386);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "explorer", "description": "Investigate #3086"}).to_string(),
            Some("toolu_done"),
        ),
    );
    // The user record closes the Task with its toolUseResult accounting. The
    // toolUseResult agentId names a rollout file that does NOT exist here, so
    // the summary must come entirely from the in-stream totals (no IO).
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_done",
                    "is_error": false
                }]
            },
            "toolUseResult": {
                "agentId": "amissingrollout000",
                "totalToolUseCount": 81,
                "totalTokens": 28824,
                "totalDurationMs": 1_140_000
            }
        })),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let line = rendered
        .lines()
        .find(|line| line.contains("explorer Investigate #3086"))
        .unwrap_or_else(|| panic!("subagent slot missing in: {rendered}"));
    // 81 tools, 28824 → 28.8k tokens, 1_140_000ms → 19m.
    assert!(
        line.contains("Done (81 tools · 28.8k tokens · 19m)"),
        "expected TUI-parity Done summary, got: {line}"
    );
    assert!(line.contains('✓'), "finished subagent must show ✓: {line}");
}

// #3086: a single-tool subagent renders the singular "1 tool" noun and small
// counts render verbatim (no k suffix), seconds under a minute stay `Xs`.
#[test]
fn status_panel_subagent_done_summary_handles_singular_and_small_values() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(387);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "tiny", "description": "Quick probe"}).to_string(),
            Some("toolu_tiny"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_tiny",
                    "is_error": false
                }]
            },
            "toolUseResult": {
                "agentId": "atinyrollout000000",
                "totalToolUseCount": 1,
                "totalTokens": 940,
                "totalDurationMs": 45_000
            }
        })),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let line = rendered
        .lines()
        .find(|line| line.contains("tiny Quick probe"))
        .unwrap_or_else(|| panic!("subagent slot missing in: {rendered}"));
    assert!(
        line.contains("Done (1 tool · 940 tokens · 45s)"),
        "expected singular/small-value summary, got: {line}"
    );
}

// #3086: a malformed/partial `toolUseResult` (string body, no accounting) must
// not panic and must not synthesize a Done summary — it falls back to the plain
// finished marker, preserving #3084 pairing behavior.
#[test]
fn status_panel_subagent_without_accounting_has_no_done_summary() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(388);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "plain", "description": "No accounting"}).to_string(),
            Some("toolu_plain"),
        ),
    );
    // toolUseResult is a bare string → not a subagent summary; the legacy Task
    // tool_result path closes the slot without a Done line.
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), false, Some("toolu_plain")),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let line = rendered
        .lines()
        .find(|line| line.contains("plain No accounting"))
        .unwrap_or_else(|| panic!("subagent slot missing in: {rendered}"));
    assert!(line.contains('✓'), "slot must still close as done: {line}");
    assert!(
        !line.contains("Done ("),
        "no accounting → no Done summary, got: {line}"
    );
}

// #3086 P1 #1: a `user` record carrying the subagent `toolUseResult` aggregate
// PLUS multiple `tool_result` blocks (the finished subagent's Task result + an
// unrelated foreground tool result) while ANOTHER subagent is still running must
// attribute the Done summary ONLY to the matching subagent slot. The unrelated
// block must NOT emit a Done/summary, and the aggregate must NOT mis-route to
// the still-running slot via the last-unfinished fallback.
#[test]
fn status_panel_subagent_summary_attaches_only_to_matching_slot() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(389);

    // Two subagents start in parallel: "done" (will finish) and "running"
    // (stays running). A foreground Bash also runs concurrently.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "finisher", "description": "Finishing work"}).to_string(),
            Some("toolu_done"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "worker", "description": "Still running"}).to_string(),
            Some("toolu_running"),
        ),
    );

    // One `user` record carries the subagent aggregate (for toolu_done) AND a
    // second, unrelated foreground tool_result in the same batch.
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_done",
                        "is_error": false
                    },
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_bash_unrelated",
                        "is_error": false
                    }
                ]
            },
            "toolUseResult": {
                "agentId": "afinisher00000000",
                "totalToolUseCount": 12,
                "totalTokens": 5000,
                "totalDurationMs": 30_000
            }
        })),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let finisher_line = rendered
        .lines()
        .find(|line| line.contains("finisher Finishing work"))
        .unwrap_or_else(|| panic!("finisher slot missing in: {rendered}"));
    let worker_line = rendered
        .lines()
        .find(|line| line.contains("worker Still running"))
        .unwrap_or_else(|| panic!("worker slot missing in: {rendered}"));

    // The matching subagent gets the Done summary and is marked done.
    assert!(
        finisher_line.contains("Done (12 tools · 5k tokens · 30s)"),
        "matching subagent must carry the Done summary, got: {finisher_line}"
    );
    assert!(
        finisher_line.contains('✓'),
        "matching subagent must be marked done, got: {finisher_line}"
    );

    // The still-running subagent must NOT be touched: no Done summary, no
    // done/fail marker. The unrelated block + the aggregate must never mis-route
    // here via the last-unfinished fallback.
    assert!(
        !worker_line.contains("Done ("),
        "running subagent must not get a stray Done summary, got: {worker_line}"
    );
    assert!(
        !worker_line.contains('✓') && !worker_line.contains('✗'),
        "running subagent must stay running (no marker), got: {worker_line}"
    );
}

// Status-panel premature-✓ bug: a subagent launched with `run_in_background`
// returns its Task `tool_result` immediately (a launch ack) while the subagent
// keeps running and outlives the launching turn. The panel must NOT mark such a
// background subagent ✓ on that ack-only end; it stays running until a GENUINE
// completion (terminal task_notification) arrives.
#[test]
fn status_panel_background_subagent_not_marked_done_on_launch_ack() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(871);

    // Background subagent launched.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "bgworker",
                "description": "Long background job",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_bg"),
        ),
    );
    // The Task tool_result fires immediately — only a launch ack. For a
    // background dispatch the subagent is still running, so this MUST NOT
    // finalize the slot.
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), false, Some("toolu_bg")),
    );

    let rendered_running =
        events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let bg_line = rendered_running
        .lines()
        .find(|line| line.contains("bgworker Long background job"))
        .unwrap_or_else(|| panic!("background subagent slot missing in: {rendered_running}"));
    assert!(
        !bg_line.contains('✓') && !bg_line.contains('✗'),
        "background subagent must stay running on the launch ack (no ✓), got: {bg_line}"
    );

    // A terminal task_notification is the real completion → now it is ✓.
    events.push_status_events(
        channel_id,
        status_events_from_task_notification("subagent", "completed", "all done"),
    );
    let rendered_done =
        events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let bg_done_line = rendered_done
        .lines()
        .find(|line| line.contains("bgworker Long background job"))
        .unwrap_or_else(|| panic!("background subagent slot missing in: {rendered_done}"));
    assert!(
        bg_done_line.contains('✓'),
        "background subagent must be ✓ after a terminal task_notification, got: {bg_done_line}"
    );
}

// #3368: the raw JSONL stream may contain an async launch-ack `toolUseResult`
// with an agentId but no accounting. That is dispatch acknowledgment, not a
// completion, so status_events_from_json must not synthesize SubagentEnd.
#[test]
fn status_events_json_async_launch_ack_does_not_close_background_subagent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(875);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "bgworker",
                "description": "Launch ack record reconstruction",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_launch_ack"),
        ),
    );

    let reconstructed = status_events_from_json(&json!({
        "type": "user",
        "message": {
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_launch_ack",
                "is_error": false
            }]
        },
        "toolUseResult": {
            "isAsync": true,
            "status": "async_launched",
            "agentId": "a31353d794c259eb9",
            "description": "...",
            "prompt": "...",
            "outputFile": "...",
            "canReadOutputFile": true
        }
    }));
    assert!(
        !reconstructed
            .iter()
            .any(|event| matches!(event, StatusEvent::SubagentEnd { .. })),
        "launch ack must not synthesize SubagentEnd: {reconstructed:?}"
    );

    events.push_status_events(channel_id, reconstructed);
    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let line = rendered
        .lines()
        .find(|line| line.contains("bgworker Launch ack record reconstruction"))
        .unwrap_or_else(|| panic!("background subagent slot missing in: {rendered}"));
    assert!(
        !line.contains('✓') && !line.contains('✗') && !line.contains("Done ("),
        "background subagent must stay running on async launch ack, got: {line}"
    );
}

// #3920: a modern async `Agent` launch carries NO `run_in_background` in the
// tool INPUT — its async-ness is known only from the launch-ack `toolUseResult`
// (`isAsync`/`status: async_launched`). The slot therefore starts foreground
// (`background: false`); before #3920 it was dropped at the very next turn
// boundary, so a long-running background Agent subagent spawned in a prior turn
// never showed on the status panel (only Bash `run_in_background` tasks did).
// The launch-ack must PROMOTE the slot to a background subagent so it SURVIVES
// turn-boundary resets and stays observable for parallel-work monitoring.
#[test]
fn status_panel_async_agent_subagent_survives_turn_boundary_after_launch_ack() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_920_001);

    // Spawning turn: Agent tool_use WITHOUT `run_in_background` → foreground slot.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Agent",
            &json!({
                "subagent_type": "general-purpose",
                "description": "Implement #3897 r4"
            })
            .to_string(),
            Some("toolu_agent_3897"),
        ),
    );

    // The async launch-ack (record-level `isAsync`/`async_launched`, no
    // accounting) arrives as a `user` record on the watcher/json path.
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_agent_3897",
                    "is_error": false
                }]
            },
            "toolUseResult": {
                "isAsync": true,
                "status": "async_launched",
                "agentId": "aee5241a0000000",
                "description": "Implement #3897 r4",
                "prompt": "...",
                "outputFile": "...",
                "canReadOutputFile": true
            }
        })),
    );

    let spawning = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(spawning.contains("Subagents"));
    assert!(
        spawning.contains("general-purpose Implement #3897 r4"),
        "async Agent subagent should render during the spawning turn: {spawning}"
    );

    // Turn boundary: the next turn resets per-turn content, preserving only
    // unfinished BACKGROUND residuals (#3386). The promoted slot must survive.
    events.clear_channel_preserving_footer_residuals(channel_id);

    let next_turn = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_300);
    let line = next_turn
        .lines()
        .find(|line| line.contains("general-purpose Implement #3897 r4"))
        .unwrap_or_else(|| {
            panic!("background Agent subagent must survive the turn boundary: {next_turn}")
        });
    assert!(
        next_turn.contains("Subagents"),
        "the carried background subagent must still render under Subagents: {next_turn}"
    );
    assert!(
        !line.contains('✓') && !line.contains('✗') && !line.contains("Done ("),
        "the carried background subagent is still running (no terminal marker): {line}"
    );
}

// #3920: surfacing the carried background subagent must NOT introduce
// per-render nondeterminism — the panel text stays byte-identical across
// heartbeat ticks when no status change occurred (#3477/#3812 invariant).
#[test]
fn status_panel_carried_background_subagent_is_heartbeat_stable() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_920_002);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Agent",
            &json!({ "subagent_type": "Explore", "description": "Audit #3864" }).to_string(),
            Some("toolu_agent_3864"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_agent_3864",
                    "is_error": false
                }]
            },
            "toolUseResult": { "isAsync": true, "status": "async_launched", "agentId": "a106f023" }
        })),
    );
    events.clear_channel_preserving_footer_residuals(channel_id);

    let first = events.render_status_panel_with_heartbeat(
        channel_id,
        &ProviderKind::Claude,
        1_700_000_000,
        1_700_000_005,
    );
    let second = events.render_status_panel_with_heartbeat(
        channel_id,
        &ProviderKind::Claude,
        1_700_000_000,
        1_700_000_090,
    );

    assert!(
        first.contains("Explore Audit #3864"),
        "carried background subagent should render: {first}"
    );
    assert_eq!(
        first, second,
        "panel text must be byte-identical across heartbeat ticks with no status change"
    );
}

#[test]
fn status_panel_async_completion_with_accounting_still_finalizes_subagent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(876);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "asyncworker",
                "description": "Completion accounting"
            })
            .to_string(),
            Some("toolu_async_done"),
        ),
    );

    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_async_done",
                    "is_error": false
                }]
            },
            "toolUseResult": {
                "agentId": "aasyncdone000000",
                "totalToolUseCount": 12,
                "totalTokens": 5000,
                "totalDurationMs": 30_000
            }
        })),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let line = rendered
        .lines()
        .find(|line| line.contains("asyncworker Completion accounting"))
        .unwrap_or_else(|| panic!("async completion slot missing in: {rendered}"));
    assert!(
        line.contains("Done (12 tools · 5k tokens · 30s)") && line.contains('✓'),
        "completion with accounting must still finalize, got: {line}"
    );
}

#[test]
fn status_panel_foreground_completion_without_agent_id_still_finalizes_subagent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(877);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "fgworker",
                "description": "No agent id completion"
            })
            .to_string(),
            Some("toolu_fg_no_agent"),
        ),
    );

    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_fg_no_agent",
                    "is_error": false
                }]
            },
            "toolUseResult": {
                "totalToolUseCount": 3,
                "totalTokens": 1500,
                "totalDurationMs": 20_000
            }
        })),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let line = rendered
        .lines()
        .find(|line| line.contains("fgworker No agent id completion"))
        .unwrap_or_else(|| panic!("foreground completion slot missing in: {rendered}"));
    assert!(
        line.contains("Done (3 tools · 1.5k tokens · 20s)") && line.contains('✓'),
        "foreground completion without agentId must still finalize, got: {line}"
    );
}

// #3359: an ack-only Task result with a non-matching tool_use_id must be
// ignored, not routed through the last-unfinished fallback. The later
// summary-bearing completion with the matching id is the first event allowed to
// mark the still-running background subagent done.
#[test]
fn status_panel_background_ack_only_unmatched_id_waits_for_matching_completion() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(874);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "bgworker",
                "description": "Background fallback guard",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_bg_real"),
        ),
    );
    events.push_status_events(
        channel_id,
        vec![StatusEvent::SubagentEnd {
            success: true,
            tool_use_id: Some("toolu_other".to_string()),
            summary: None,
            ack_only: true,
        }],
    );

    let rendered_running =
        events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let running_line = rendered_running
        .lines()
        .find(|line| line.contains("bgworker Background fallback guard"))
        .unwrap_or_else(|| panic!("background subagent slot missing in: {rendered_running}"));
    assert!(
        !running_line.contains('✓') && !running_line.contains('✗'),
        "unmatched ack-only end must leave background slot running, got: {running_line}"
    );

    events.push_status_events(
        channel_id,
        vec![StatusEvent::SubagentEnd {
            success: true,
            tool_use_id: Some("toolu_bg_real".to_string()),
            summary: Some(crate::services::agent_protocol::SubagentSummary {
                tool_count: Some(3),
                tokens: Some(1_200),
                duration_secs: Some(42),
            }),
            ack_only: false,
        }],
    );

    let rendered_done =
        events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let done_line = rendered_done
        .lines()
        .find(|line| line.contains("bgworker Background fallback guard"))
        .unwrap_or_else(|| panic!("background subagent slot missing in: {rendered_done}"));
    assert!(
        done_line.contains("Done (3 tools · 1.2k tokens · 42s)"),
        "matching summary completion must attach accounting, got: {done_line}"
    );
    assert!(
        done_line.contains('✓'),
        "matching summary completion must mark the slot done, got: {done_line}"
    );
}

// #3359 hole 2: an id-bearing ack-only end with no matching slot must not
// finalize any unfinished slot via fallback, whether the candidate slot is
// background or foreground.
#[test]
fn status_panel_ack_only_unmatched_id_does_not_fallback_to_any_slot() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(875);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "bgworker",
                "description": "Still background",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_bg"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "fgworker", "description": "Still foreground"}).to_string(),
            Some("toolu_fg"),
        ),
    );
    events.push_status_events(
        channel_id,
        vec![StatusEvent::SubagentEnd {
            success: true,
            tool_use_id: Some("toolu_wrong".to_string()),
            summary: None,
            ack_only: true,
        }],
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    for expected in ["bgworker Still background", "fgworker Still foreground"] {
        let line = rendered
            .lines()
            .find(|line| line.contains(expected))
            .unwrap_or_else(|| panic!("subagent slot missing in: {rendered}"));
        assert!(
            !line.contains('✓') && !line.contains('✗'),
            "unmatched ack-only end must not fallback-finalize {expected}, got: {line}"
        );
    }
}

// Foreground subagents still close on their genuine summary-bearing completion.
#[test]
fn status_panel_foreground_subagent_summary_completion_still_marks_done() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(876);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "fgworker", "description": "Summary completion"}).to_string(),
            Some("toolu_fg_summary"),
        ),
    );
    events.push_status_events(
        channel_id,
        vec![StatusEvent::SubagentEnd {
            success: true,
            tool_use_id: Some("toolu_fg_summary".to_string()),
            summary: Some(crate::services::agent_protocol::SubagentSummary {
                tool_count: Some(2),
                tokens: Some(900),
                duration_secs: Some(11),
            }),
            ack_only: false,
        }],
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let line = rendered
        .lines()
        .find(|line| line.contains("fgworker Summary completion"))
        .unwrap_or_else(|| panic!("foreground subagent slot missing in: {rendered}"));
    assert!(
        line.contains("Done (2 tools · 900 tokens · 11s)"),
        "foreground summary completion must keep Done summary, got: {line}"
    );
    assert!(
        line.contains('✓'),
        "foreground summary completion must mark the slot done, got: {line}"
    );
}

// Edge case of the premature-✓ fix: a `run_in_background` LAUNCH that FAILS
// (the Task `tool_result` is an error — the subagent never started) is TERMINAL,
// not a launch ack. The slot must finalize as failed (✗) instead of being stuck
// 'running' forever. Guards against the ack-only suppression swallowing a failed
// background launch.
#[test]
fn status_panel_background_subagent_failed_launch_marked_failed() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(873);

    // Background subagent launched.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "bgworker",
                "description": "Doomed background job",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_bg_fail"),
        ),
    );
    // The Task tool_result returns an ERROR: the background launch FAILED, the
    // subagent never started. This is terminal — the slot must render ✗, not
    // stay stuck running.
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), true, Some("toolu_bg_fail")),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let bg_line = rendered
        .lines()
        .find(|line| line.contains("bgworker Doomed background job"))
        .unwrap_or_else(|| panic!("background subagent slot missing in: {rendered}"));
    assert!(
        bg_line.contains('✗'),
        "failed background launch must finalize as ✗, got: {bg_line}"
    );
    assert!(
        !bg_line.contains('✓'),
        "failed background launch must not be marked ✓, got: {bg_line}"
    );
}

// A FOREGROUND subagent's Task tool_result IS its real completion, so the
// ack-only end still finalizes it (✓). Guards against the background fix
// regressing the foreground path.
#[test]
fn status_panel_foreground_subagent_marked_done_on_tool_result() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(872);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "fgworker", "description": "Quick job"}).to_string(),
            Some("toolu_fg"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), false, Some("toolu_fg")),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let fg_line = rendered
        .lines()
        .find(|line| line.contains("fgworker Quick job"))
        .unwrap_or_else(|| panic!("foreground subagent slot missing in: {rendered}"));
    assert!(
        fg_line.contains('✓'),
        "foreground subagent must be ✓ on its tool_result, got: {fg_line}"
    );
}

// #3086 P1: a single `user` record may BATCH multiple finished subagents, each
// `tool_result` block carrying its OWN `toolUseResult` aggregate. Each Done
// summary must land on ITS OWN slot (keyed by that block's tool_use_id), not all
// on the first id-bearing block. Subagent A (tuA, summaryA) and subagent B (tuB,
// summaryB) both finish in one batched record: A's summary → slot tuA, B's → tuB.
#[test]
fn status_panel_batched_multi_subagent_summaries_land_on_own_slots() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(392);

    // Two subagents start in parallel.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "alpha", "description": "Task A"}).to_string(),
            Some("toolu_a"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "beta", "description": "Task B"}).to_string(),
            Some("toolu_b"),
        ),
    );

    // ONE `user` record batches BOTH finished subagents. Each `tool_result`
    // block carries its OWN `toolUseResult` aggregate (its own agentId/total*).
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_a",
                        "is_error": false,
                        "toolUseResult": {
                            "agentId": "aalpha00000000000",
                            "totalToolUseCount": 12,
                            "totalTokens": 5000,
                            "totalDurationMs": 30_000
                        }
                    },
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_b",
                        "is_error": false,
                        "toolUseResult": {
                            "agentId": "abeta000000000000",
                            "totalToolUseCount": 81,
                            "totalTokens": 28824,
                            "totalDurationMs": 1_140_000
                        }
                    }
                ]
            }
        })),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let alpha_line = rendered
        .lines()
        .find(|line| line.contains("alpha Task A"))
        .unwrap_or_else(|| panic!("alpha slot missing in: {rendered}"));
    let beta_line = rendered
        .lines()
        .find(|line| line.contains("beta Task B"))
        .unwrap_or_else(|| panic!("beta slot missing in: {rendered}"));

    // A's aggregate lands on A's slot (tuA), B's on B's slot (tuB) — NOT both on
    // the first block.
    assert!(
        alpha_line.contains("Done (12 tools · 5k tokens · 30s)"),
        "alpha must carry its OWN summary, got: {alpha_line}"
    );
    assert!(
        alpha_line.contains('✓'),
        "alpha must be marked done, got: {alpha_line}"
    );
    assert!(
        beta_line.contains("Done (81 tools · 28.8k tokens · 19m)"),
        "beta must carry its OWN summary, got: {beta_line}"
    );
    assert!(
        beta_line.contains('✓'),
        "beta must be marked done, got: {beta_line}"
    );
}

// #3086 P1 #1: a summary-bearing `SubagentEnd` whose `tool_use_id` matches NO
// tracked slot must be dropped, not mis-routed to the last unfinished slot.
#[test]
fn status_panel_unmatched_summary_end_is_dropped_not_misrouted() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(390);

    // A single running subagent with a known id.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "worker", "description": "Long task"}).to_string(),
            Some("toolu_real"),
        ),
    );

    // A summary-bearing end arrives with an id that does NOT match the slot.
    events.push_status_events(
        channel_id,
        vec![StatusEvent::SubagentEnd {
            success: true,
            tool_use_id: Some("toolu_ghost".to_string()),
            summary: Some(crate::services::agent_protocol::SubagentSummary {
                tool_count: Some(99),
                tokens: Some(99_999),
                duration_secs: Some(99),
            }),
            ack_only: false,
        }],
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let worker_line = rendered
        .lines()
        .find(|line| line.contains("worker Long task"))
        .unwrap_or_else(|| panic!("worker slot missing in: {rendered}"));
    assert!(
        !worker_line.contains("Done ("),
        "unmatched summary must not land on the running slot, got: {worker_line}"
    );
    assert!(
        !worker_line.contains('✓') && !worker_line.contains('✗'),
        "unmatched summary-bearing end must not close the slot, got: {worker_line}"
    );
}

// #3086 P1 #2: the hot-path summary extraction (`subagent_summary_from_user_record`
// via `status_events_from_json`) must rely ONLY on the in-stream `toolUseResult`
// aggregate — no synchronous rollout file read. With `cwd`/`sessionId`/`agentId`
// present but accounting fields missing, the previous code would have read a
// rollout file off disk; now it must return a partial summary from in-stream
// fields alone (no IO), omitting the missing parts.
#[test]
fn status_panel_subagent_summary_no_rollout_io_on_hot_path() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(391);

    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({"subagent_type": "partial", "description": "Partial accounting"}).to_string(),
            Some("toolu_partial"),
        ),
    );

    // Aggregate has agentId + cwd + sessionId (the old fallback trigger) and
    // ONLY tool_count — tokens/duration are absent. No rollout file is read, so
    // the missing fields are simply omitted from the Done line.
    events.push_status_events(
        channel_id,
        status_events_from_json(&json!({
            "type": "user",
            "cwd": "/tmp/some/project",
            "sessionId": "f525f356-9cf1-4c45-b992-4e1210ee68be",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_partial",
                    "is_error": false
                }]
            },
            "toolUseResult": {
                "agentId": "apartialrollout00",
                "totalToolUseCount": 7
            }
        })),
    );

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let line = rendered
        .lines()
        .find(|line| line.contains("partial Partial accounting"))
        .unwrap_or_else(|| panic!("subagent slot missing in: {rendered}"));
    // Only the in-stream tool_count survives; tokens/duration are omitted (no IO
    // fallback computed them).
    assert!(
        line.contains("Done (7 tools)"),
        "partial in-stream summary expected, got: {line}"
    );
    assert!(
        !line.contains("tokens") && !line.contains('·'),
        "no rollout-derived fields should appear (no IO), got: {line}"
    );
    assert!(line.contains('✓'), "slot must still close as done: {line}");
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
    // Drop the DashMap shard Ref too: the pushes below target channel 2896,
    // whose `entry()` needs the shard WRITE lock. With the per-instance random
    // hasher, 2895/2896 sometimes share a shard — holding this read guard
    // across the pushes then self-deadlocks the test (observed hang).
    drop(status_entry);

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
    // A FAILED Task tool_result is terminal (the launch errored — the subagent
    // never ran), so it is NOT ack-only: the panel must finalize the slot (✗)
    // rather than keep a background slot 'running' forever.
    assert_eq!(
        status_events_from_tool_result(Some("Task"), true),
        vec![
            StatusEvent::ToolEnd { success: false },
            StatusEvent::SubagentEnd {
                success: false,
                tool_use_id: None,
                summary: None,
                ack_only: false
            }
        ]
    );
    // A SUCCESSFUL Task tool_result is the (possibly background) launch ack →
    // ack_only so a still-running background subagent is not prematurely ✓.
    assert_eq!(
        status_events_from_tool_result(Some("Task"), false),
        vec![
            StatusEvent::ToolEnd { success: true },
            StatusEvent::SubagentEnd {
                success: true,
                tool_use_id: None,
                summary: None,
                ack_only: true
            }
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

// ---------------------------------------------------------------------------
// #3393: user-record `<task-notification>` XML → live panel terminal StatusEvents.
//
// Background-task / subagent completions reach the transcript ONLY as this XML
// (never the stream-json `system` record the panel's `system_status_events`
// parses). These tests drive the FULL ingestion: real-shape XML text (incl. the
// hyphenated `<tool-use-id>` from the live transcript) → bridge parse → push →
// `render_completion_footer` shows ✓. State-machine-only proof is explicitly
// forbidden by the issue, so every assertion goes through the panel render.
// ---------------------------------------------------------------------------

#[test]
fn task_notification_xml_background_bash_flips_footer_slot_to_done() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_393_001);
    // A running background Bash slot keyed by the launch tool-use id.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "gh run watch",
                "description": "Wait until PR 3392 CI settles",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_01Ls2svfdnzcn9uGwA7aHjHW"),
            true,
        ),
    );
    let running = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let running_block = running
        .block
        .expect("running background Bash should render");
    assert!(running.has_unfinished_entries);
    assert!(running_block.contains("Bash Wait until PR 3392 CI settles ⠸"));
    assert!(!running_block.contains('✓'));

    // Real-shape XML user-record text (background Bash variant, hyphenated
    // `<tool-use-id>` child tag, status `completed`) — copied from a live
    // transcript notification.
    let raw = "<task-notification>\n\
        <task-id>b5gr0v9xj</task-id>\n\
        <tool-use-id>toolu_01Ls2svfdnzcn9uGwA7aHjHW</tool-use-id>\n\
        <output-file>/private/tmp/claude-501/sess/tasks/b5gr0v9xj.output</output-file>\n\
        <status>completed</status>\n\
        <summary>Background command \"Wait until PR 3392 CI settles\" completed (exit code 0)</summary>\n\
        </task-notification>";
    let bridged = status_events_from_task_notification_xml_for_footer_mode(raw, true);
    assert!(
        bridged
            .iter()
            .any(|e| matches!(e, StatusEvent::BackgroundTaskEnd { .. })),
        "background XML must bridge to BackgroundTaskEnd: {bridged:?}"
    );
    events.push_status_events(channel_id, bridged);

    let done = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    let done_block = done
        .block
        .expect("finished background Bash should stay visible");
    assert!(!done.has_unfinished_entries);
    assert!(
        done_block.contains("Bash Wait until PR 3392 CI settles ✓"),
        "matching XML notification must flip ✓: {done_block}"
    );
    assert!(!done_block.contains('⠼'));
}

#[test]
fn task_notification_xml_subagent_flips_footer_slot_to_done() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_393_002);
    // A running Task subagent slot keyed by its launch tool-use id.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "scout",
                "description": "Scout issues #3275 #3276",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_018F3HtbweDDNEbi44HKAhhi"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(
            Some("Task"),
            false,
            Some("toolu_018F3HtbweDDNEbi44HKAhhi"),
        ),
    );
    let running = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let running_block = running.block.expect("running subagent should render");
    assert!(running.has_unfinished_entries);
    assert!(running_block.contains("Subagents"));
    assert!(!running_block.contains('✓'));

    // Real-shape subagent variant: summary prefix `Agent "…" completed`, the
    // SAME hyphenated `<tool-use-id>` as the launch — bridges to `SubagentEnd`.
    let raw = "<task-notification>\n\
        <task-id>a09e45d12a68015a5</task-id>\n\
        <tool-use-id>toolu_018F3HtbweDDNEbi44HKAhhi</tool-use-id>\n\
        <output-file>/private/tmp/claude-501/sess/tasks/a09e45d12a68015a5.output</output-file>\n\
        <status>completed</status>\n\
        <summary>Agent \"Scout issues #3275 #3276\" completed</summary>\n\
        <result>Done.</result>\n\
        </task-notification>";
    let bridged = status_events_from_task_notification_xml_for_footer_mode(raw, true);
    assert!(
        bridged.iter().any(|e| matches!(
            e,
            StatusEvent::SubagentEnd {
                ack_only: false,
                ..
            }
        )),
        "subagent XML must bridge to a finalizing SubagentEnd: {bridged:?}"
    );
    events.push_status_events(channel_id, bridged);

    let done = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    let done_block = done.block.expect("finished subagent should stay visible");
    assert!(!done.has_unfinished_entries);
    assert!(
        done_block.contains("Scout issues #3275 #3276") && done_block.contains('✓'),
        "matching subagent XML notification must flip ✓: {done_block}"
    );
}

#[test]
fn task_notification_xml_unknown_id_and_duplicate_and_nonterminal_are_safe() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_393_003);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "deploy.sh",
                "description": "Deploy runtime",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_known"),
            true,
        ),
    );

    // (a) Unknown tool-use-id: terminal End for a slot we never opened — no-op.
    let unknown = "<task-notification><task-id>x1</task-id>\
        <tool-use-id>toolu_unknown</tool-use-id><status>completed</status>\
        <summary>Background command \"Other\" completed (exit code 0)</summary></task-notification>";
    events.push_status_events(
        channel_id,
        status_events_from_task_notification_xml_for_footer_mode(unknown, true),
    );
    let after_unknown = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let block = after_unknown.block.expect("known slot still renders");
    assert!(
        block.contains("Bash Deploy runtime ⠸") && !block.contains('✓'),
        "unknown-id notification must not flip the known slot: {block}"
    );

    // (b) Non-terminal status: produces NO terminal End event.
    let nonterminal = "<task-notification><task-id>x2</task-id>\
        <tool-use-id>toolu_known</tool-use-id><status>running</status>\
        <summary>Background command \"Deploy runtime\" running</summary></task-notification>";
    let nonterminal_events =
        status_events_from_task_notification_xml_for_footer_mode(nonterminal, true);
    assert!(
        !nonterminal_events
            .iter()
            .any(|e| matches!(e, StatusEvent::BackgroundTaskEnd { .. })),
        "non-terminal status must not bridge a BackgroundTaskEnd: {nonterminal_events:?}"
    );
    events.push_status_events(channel_id, nonterminal_events);
    let still_running = events
        .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
        .block
        .expect("slot still renders");
    assert!(
        !still_running.contains('✓'),
        "non-terminal must not flip ✓: {still_running}"
    );

    // (c) Terminal match flips ✓; a DUPLICATE terminal notification must not flip
    // the slot back to running.
    let done_xml = "<task-notification><task-id>x3</task-id>\
        <tool-use-id>toolu_known</tool-use-id><status>completed</status>\
        <summary>Background command \"Deploy runtime\" completed (exit code 0)</summary></task-notification>";
    events.push_status_events(
        channel_id,
        status_events_from_task_notification_xml_for_footer_mode(done_xml, true),
    );
    events.push_status_events(
        channel_id,
        status_events_from_task_notification_xml_for_footer_mode(done_xml, true),
    );
    let done = events
        .render_completion_footer(channel_id, &ProviderKind::Claude, "⠼")
        .block
        .expect("done slot renders");
    assert!(
        done.contains("Bash Deploy runtime ✓") && !done.contains('⠼'),
        "duplicate terminal notification must stay ✓, not flip back: {done}"
    );
}

#[test]
fn task_notification_xml_bridge_inert_when_footer_mode_off() {
    // Footer-mode OFF → the bridge yields no events so the legacy separate-panel
    // render path is untouched.
    let raw = "<task-notification><task-id>off1</task-id>\
        <tool-use-id>toolu_off</tool-use-id><status>completed</status>\
        <summary>Background command \"x\" completed (exit code 0)</summary></task-notification>";
    let bridged = status_events_from_task_notification_xml_for_footer_mode(raw, false);
    assert!(
        bridged.is_empty(),
        "footer-mode-off bridge must be inert: {bridged:?}"
    );
}

#[test]
fn task_completion_card_suppression_requires_footer_mode_and_matching_background_slot() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_654_001);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id_for_footer_mode(
            "Bash",
            &json!({
                "command": "gh run watch",
                "description": "Watch CI",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_bg_match"),
            true,
        ),
    );

    let completed = "<task-notification><task-id>bg1</task-id>\
        <tool-use-id>toolu_bg_match</tool-use-id><status>completed</status>\
        <summary>Background command \"Watch CI\" completed (exit code 0)</summary></task-notification>";
    assert!(
        events
            .task_notification_completion_visible_in_footer_for_mode(channel_id, completed, true,),
        "footer-on matching background completion should suppress the notify card"
    );
    assert!(
        !events
            .task_notification_completion_visible_in_footer_for_mode(channel_id, completed, false,),
        "footer-off/legacy mode must keep the notify card"
    );

    let unknown = "<task-notification><task-id>bg2</task-id>\
        <tool-use-id>toolu_bg_unknown</tool-use-id><status>completed</status>\
        <summary>Background command \"Other\" completed (exit code 0)</summary></task-notification>";
    assert!(
        !events.task_notification_completion_visible_in_footer_for_mode(channel_id, unknown, true),
        "unknown tool-use-id has no footer completion surface, so the card must remain"
    );

    let failed = "<task-notification><task-id>bg3</task-id>\
        <tool-use-id>toolu_bg_match</tool-use-id><status>failed</status>\
        <summary>Background command \"Watch CI\" failed (exit code 1)</summary></task-notification>";
    assert!(
        !events.task_notification_completion_visible_in_footer_for_mode(channel_id, failed, true),
        "failure/error notifications keep their card for details instead of being footer-suppressed"
    );
}

#[test]
fn task_completion_card_suppression_requires_idful_matching_subagent_slot() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_654_002);
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "scout",
                "description": "Scout issue #3654",
                "run_in_background": true
            })
            .to_string(),
            Some("toolu_subagent_match"),
        ),
    );
    events.push_status_events(
        channel_id,
        status_events_from_tool_result_with_id(Some("Task"), false, Some("toolu_subagent_match")),
    );

    let completed = "<task-notification><task-id>sub1</task-id>\
        <tool-use-id>toolu_subagent_match</tool-use-id><status>completed</status>\
        <summary>Agent \"Scout issue #3654\" completed</summary></task-notification>";
    assert!(
        events
            .task_notification_completion_visible_in_footer_for_mode(channel_id, completed, true,),
        "matching idful subagent completion should suppress the duplicate card"
    );

    let idless = "<task-notification><task-id>sub2</task-id><status>completed</status>\
        <summary>Agent \"Scout issue #3654\" completed</summary></task-notification>";
    assert!(
        !events.task_notification_completion_visible_in_footer_for_mode(channel_id, idless, true),
        "id-less subagent completion cannot safely map to a footer slot"
    );
}

// #3393 finding 1: an id-LESS subagent `<task-notification>` XML (no
// `<tool-use-id>` child) must produce NO terminal effect. Before the fix the XML
// bridge emitted `SubagentEnd { tool_use_id: None }`, the panel fell back to "the
// last unfinished subagent slot", and the WRONG slot was finalized (and, with
// #3391, evicted on delivery). The bridge now drops id-less terminal ends.
#[test]
fn task_notification_xml_idless_subagent_does_not_flip_or_evict_a_slot() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_393_004);
    // A running FOREGROUND Task subagent slot keyed by its launch tool-use id.
    // Foreground (not background) so an id-less ack-only fallback WOULD finalize
    // it pre-fix — the strongest exposure of the bug.
    events.push_status_events(
        channel_id,
        status_events_from_tool_use_with_id(
            "Task",
            &json!({
                "subagent_type": "scout",
                "description": "Scout issue #3393"
            })
            .to_string(),
            Some("toolu_live_slot"),
        ),
    );

    // Real-shape subagent variant WITHOUT a `<tool-use-id>` child (some repeats /
    // lost-process notifications omit it). Terminal status `completed`.
    let idless = "<task-notification>\n\
        <task-id>idless1</task-id>\n\
        <status>completed</status>\n\
        <summary>Agent \"some other agent\" completed</summary>\n\
        <result>Done.</result>\n\
        </task-notification>";
    let bridged = status_events_from_task_notification_xml_for_footer_mode(idless, true);
    assert!(
        !bridged
            .iter()
            .any(|e| matches!(e, StatusEvent::SubagentEnd { .. })),
        "id-less subagent XML must NOT bridge a SubagentEnd: {bridged:?}"
    );
    events.push_status_events(channel_id, bridged);

    // The slot is untouched: still present and still unfinished (no eviction, no
    // ✓/✗ flip onto the wrong slot).
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            guard.subagents.len(),
            1,
            "id-less subagent notification must not evict the slot"
        );
        assert!(
            guard.subagents[0].finished.is_none(),
            "id-less subagent notification must leave the slot unfinished"
        );
    }

    let footer = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    assert!(
        footer.has_unfinished_entries,
        "the running subagent slot must remain unfinished"
    );
    let block = footer.block.expect("running subagent should render");
    assert!(
        !block.contains('✓'),
        "id-less subagent notification must not flip ✓: {block}"
    );
}

// #3393 finding 3: a workflow `<task-notification>` XML with a NON-terminal
// status (e.g. running) must NOT emit `WorkflowEnd`; terminal statuses still map
// success via `!is_error`, consistent with the subagent/background arms.
#[test]
fn task_notification_xml_workflow_gates_workflow_end_on_terminal_status() {
    let running = "<task-notification><task-id>wf1</task-id><status>running</status>\
        <summary>Dynamic workflow \"probe\" running</summary></task-notification>";
    let running_events = status_events_from_task_notification_xml_for_footer_mode(running, true);
    assert!(
        !running_events
            .iter()
            .any(|e| matches!(e, StatusEvent::WorkflowEnd { .. })),
        "status=running workflow XML must NOT emit WorkflowEnd: {running_events:?}"
    );

    let completed = "<task-notification><task-id>wf2</task-id><status>completed</status>\
        <summary>Dynamic workflow \"probe\" completed</summary></task-notification>";
    let completed_events =
        status_events_from_task_notification_xml_for_footer_mode(completed, true);
    assert!(
        completed_events
            .iter()
            .any(|e| matches!(e, StatusEvent::WorkflowEnd { success: true, .. })),
        "status=completed workflow XML must emit WorkflowEnd{{success:true}}: {completed_events:?}"
    );

    let failed = "<task-notification><task-id>wf3</task-id><status>failed</status>\
        <summary>Dynamic workflow \"probe\" failed</summary></task-notification>";
    let failed_events = status_events_from_task_notification_xml_for_footer_mode(failed, true);
    assert!(
        failed_events
            .iter()
            .any(|e| matches!(e, StatusEvent::WorkflowEnd { success: false, .. })),
        "status=failed workflow XML must emit WorkflowEnd{{success:false}}: {failed_events:?}"
    );
}

#[test]
fn workflow_completion_card_never_suppressed_because_footer_lacks_workflow_section() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_654_003);

    let completed = "<task-notification><task-id>wf-visible</task-id><status>completed</status>\
        <summary>Dynamic workflow \"probe\" completed</summary></task-notification>";
    assert!(
        !events
            .task_notification_completion_visible_in_footer_for_mode(channel_id, completed, true,),
        "completion footers do not render Workflow sections, so workflow completion cards must stay"
    );
    assert!(
        !events
            .task_notification_completion_visible_in_footer_for_mode(channel_id, completed, false,),
        "footer-off/legacy mode must keep workflow completion cards"
    );

    let running = "<task-notification><task-id>wf-running</task-id><status>running</status>\
        <summary>Dynamic workflow \"probe\" running</summary></task-notification>";
    assert!(
        !events.task_notification_completion_visible_in_footer_for_mode(channel_id, running, true),
        "non-terminal workflow notifications are not duplicate completion cards"
    );

    let failed = "<task-notification><task-id>wf-failed</task-id><status>failed</status>\
        <summary>Dynamic workflow \"probe\" failed</summary></task-notification>";
    assert!(
        !events.task_notification_completion_visible_in_footer_for_mode(channel_id, failed, true),
        "failed workflow notifications keep their card for details"
    );
}

// #3394: fence-safe truncation regression coverage.

fn fence_count(text: &str) -> usize {
    text.matches("```").count()
}

/// #3394 (1): when the joined panel exceeds the limit, the trailing fenced
/// Recent block must be DROPPED WHOLE — not chopped into a dangling ```text — and
/// the earlier sections must survive intact, with an even (balanced) fence count.
#[test]
fn truncate_panel_drops_trailing_fenced_section_whole_when_over_limit() {
    let tasks = format!("Tasks\n{}", "T".repeat(880));
    let subagents = format!("Subagents\n{}", "S".repeat(880));
    let recent = format!("🖥️ Recent\n```text\n{}\n```", "R".repeat(400));
    let sections = vec![tasks.clone(), subagents.clone(), recent];
    // Precondition: the full join overflows, but Tasks+Subagents alone fit — so
    // dropping ONLY the trailing fenced Recent block is the correct degradation.
    assert!(sections.join("\n\n").chars().count() > STATUS_PANEL_MAX_CHARS);
    assert!(
        format!("{tasks}\n\n{subagents}").chars().count() <= STATUS_PANEL_MAX_CHARS,
        "fixture sizing: earlier sections must fit once Recent is dropped"
    );

    let rendered = truncate_status_panel_sections(sections);

    assert!(rendered.chars().count() <= STATUS_PANEL_MAX_CHARS);
    // No unterminated fence and no literal dangling ```text.
    assert_eq!(fence_count(&rendered) % 2, 0, "odd fence count: {rendered}");
    assert!(!rendered.contains("```text"), "literal ```text leaked");
    // Degradation is visible: Recent gone, earlier sections kept verbatim.
    assert!(!rendered.contains("🖥️ Recent"), "Recent not dropped");
    assert!(rendered.contains(&tasks), "Tasks section was cut");
    assert!(rendered.contains(&subagents), "Subagents section was cut");
}

/// #3394 (2): a single fenced section that ALONE exceeds the limit can't be
/// dropped (nothing else to shed), so it is fence-safe truncated — never left
/// with a dangling opener.
#[test]
fn truncate_panel_fence_safe_when_single_section_overflows() {
    let oversized = format!(
        "🖥️ Recent\n```text\n{}\n```",
        "X".repeat(STATUS_PANEL_MAX_CHARS + 200)
    );
    let rendered = truncate_status_panel_sections(vec![oversized]);

    assert!(rendered.chars().count() <= STATUS_PANEL_MAX_CHARS);
    assert_eq!(fence_count(&rendered) % 2, 0, "odd fence count: {rendered}");
}

/// #3394 (3): parity helper — balanced/odd, exact boundary, and the Discord
/// no-nesting semantic (a ``` INSIDE a fenced block CLOSES it).
#[test]
fn repair_fence_parity_unit_cases() {
    use crate::services::discord::single_message_panel::repair_fence_parity;
    // Balanced input is returned unchanged.
    let balanced = "a\n```text\nbody\n```\ntail";
    assert_eq!(repair_fence_parity(balanced), balanced);
    // No fences at all is unchanged.
    assert_eq!(repair_fence_parity("plain text"), "plain text");
    // Odd (dangling opener): the opener and everything after it is removed.
    let odd = "header\n\n```text\nchopped body";
    let repaired = repair_fence_parity(odd);
    assert_eq!(fence_count(&repaired) % 2, 0);
    assert!(!repaired.contains("```"));
    assert_eq!(repaired, "header");
    // Fence at the exact end (closer present) stays balanced/unchanged.
    let closed = "```text\nx\n```";
    assert_eq!(repair_fence_parity(closed), closed);
    // Three fences (open/close/open) — the third is a dangling opener and is
    // dropped; the first open+close pair (no nesting) is preserved.
    let three = "```text\nfirst\n```\nmid\n```text\nsecond";
    let repaired_three = repair_fence_parity(three);
    assert_eq!(fence_count(&repaired_three) % 2, 0);
    assert_eq!(repaired_three, "```text\nfirst\n```\nmid");
}

/// #3394 (3): a fence that LOOKS nested is a closer under Discord semantics, so
/// a four-fence sequence is balanced and must be left untouched.
#[test]
fn repair_fence_parity_treats_inner_fence_as_closer() {
    use crate::services::discord::single_message_panel::repair_fence_parity;
    let four = "```\nouter\n```\n```\nsecond\n```";
    assert_eq!(repair_fence_parity(four), four);
    assert_eq!(fence_count(four) % 2, 0);
}

/// #3394 (3): the in-turn LIVE panel routes through the protected truncation
/// path. With bloated Tasks/Subagents PLUS a fenced Recent live block (the
/// reported screenshot shape), the rendered panel stays under the limit and never
/// exposes a dangling fence (the ``` count is always even, balanced or dropped).
#[test]
fn live_status_panel_never_leaks_dangling_fence_when_bloated() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3394);

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
    }
    // Fenced Recent live block (mirrors recent_events.rs ```text fence).
    for idx in 0..6 {
        events.push_event(
            channel_id,
            RecentPlaceholderEvent::tool_use("Bash", &format!(r#"{{"command":"echo {idx}"}}"#))
                .unwrap(),
        );
    }

    let rendered = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    assert!(rendered.chars().count() <= STATUS_PANEL_MAX_CHARS);
    assert_eq!(
        fence_count(&rendered) % 2,
        0,
        "live panel leaked an unterminated fence: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// #3402: transcript-driven footer-panel slot rehydration after a restart.
// ---------------------------------------------------------------------------

/// A Claude `assistant` tool_use record launching a (foreground) subagent.
fn transcript_subagent_start(tool_use_id: &str, desc: &str) -> String {
    json!({
        "type": "assistant",
        "message": {
            "content": [{
                "type": "tool_use",
                "id": tool_use_id,
                "name": "Task",
                "input": { "subagent_type": "explorer", "description": desc }
            }]
        }
    })
    .to_string()
}

/// A Claude `assistant` tool_use record launching a background Bash task.
fn transcript_background_bash_start(tool_use_id: &str, desc: &str) -> String {
    json!({
        "type": "assistant",
        "message": {
            "content": [{
                "type": "tool_use",
                "id": tool_use_id,
                "name": "Bash",
                "input": { "command": "sleep 600", "description": desc, "run_in_background": true }
            }]
        }
    })
    .to_string()
}

/// A `<task-notification>` `user` record (the #3393 XML path) marking a subagent
/// completed, keyed by its launching tool-use-id.
fn transcript_subagent_completion(tool_use_id: &str) -> String {
    let xml = format!(
        "<task-notification><tool-use-id>{tool_use_id}</tool-use-id>\
         <status>completed</status><summary>Agent \"explorer\" completed</summary>\
         </task-notification>"
    );
    json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": xml }] }
    })
    .to_string()
}

/// A compaction boundary record (`isCompactSummary: true`).
fn transcript_compact_boundary() -> String {
    json!({
        "type": "user",
        "isCompactSummary": true,
        "message": { "role": "user", "content": "This session is being continued from a previous conversation" }
    })
    .to_string()
}

fn write_transcript(lines: &[String]) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::write(file.path(), body).unwrap();
    file
}

/// #3436: a watcher reconnect (`record_tmux_watcher_reconnect`) purges the
/// channel footer via `clear_channel`, so background task / subagent slots whose
/// terminal events died with the prior generation do not linger as zombie
/// spinners. Both unfinished-background slot kinds must be dropped.
#[test]
fn clear_channel_purges_unfinished_background_zombie_slots_3436() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_436_001);
    push_background_bash_task(&events, channel_id, "tailing logs", "toolu_bg_zombie");
    events.push_status_event(
        channel_id,
        StatusEvent::SubagentStart {
            subagent_type: Some("reviewer".to_string()),
            desc: Some("never finishes".to_string()),
            tool_use_id: Some("toolu_sub_zombie".to_string()),
            background: true,
        },
    );
    let before = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    assert!(
        before
            .block
            .is_some_and(|block| block.contains("tailing logs")),
        "unfinished background task should render as live before the reconnect"
    );

    // The prior generation owning these slots is dead; reconnect purges them.
    events.clear_channel(channel_id);

    assert!(
        events
            .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
            .block
            .is_none(),
        "post-reconnect footer must drop the dead generation's zombie slots"
    );
}

#[test]
fn rehydration_restores_only_unmatched_starts_after_restart() {
    // Slots present in the live process, then a restart wipes them.
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_402_001);
    push_background_bash_task(&events, channel_id, "running bg", "toolu_bg_run");
    // Simulate the restart by resetting (clearing) the channel registry.
    events.clear_channel(channel_id);
    assert!(
        events
            .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
            .block
            .is_none(),
        "post-restart footer should start empty"
    );

    // Fixture transcript: one completed subagent (matched pair), one still-running
    // subagent (unmatched), and one still-running background Bash (unmatched).
    let transcript = write_transcript(&[
        transcript_subagent_start("toolu_done", "finished work"),
        transcript_subagent_completion("toolu_done"),
        transcript_subagent_start("toolu_live", "still exploring"),
        transcript_background_bash_start("toolu_bg_live", "tailing logs"),
    ]);

    let outcome = events.rehydrate_slots_from_transcript_tail_for_footer_mode(
        channel_id,
        transcript.path(),
        true,
    );
    assert_eq!(outcome.subagents, 1, "only the unmatched subagent restored");
    assert_eq!(outcome.background_tasks, 1, "the bg task restored");

    let render = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let block = render
        .block
        .expect("rehydrated slots should render a footer");
    assert!(
        block.contains("still exploring"),
        "unmatched subagent present: {block}"
    );
    assert!(
        block.contains("tailing logs"),
        "unmatched bg task present: {block}"
    );
    assert!(
        !block.contains("finished work"),
        "completed pair absent: {block}"
    );
    assert!(render.has_unfinished_entries);
}

#[test]
fn rehydration_end_after_rehydrate_flips_check_and_evicts() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_402_002);
    let transcript = write_transcript(&[
        transcript_subagent_start("toolu_sa", "long subagent"),
        transcript_background_bash_start("toolu_bg", "bg worker"),
    ]);
    events.rehydrate_slots_from_transcript_tail_for_footer_mode(
        channel_id,
        transcript.path(),
        true,
    );

    // The #3393 bridge delivers terminal Ends for the rehydrated ids.
    events.push_status_events(
        channel_id,
        status_events_from_task_notification_with_tool_use_id(
            "subagent",
            "completed",
            "Agent done",
            Some("toolu_sa"),
        ),
    );
    complete_background_bash_task(&events, channel_id, "toolu_bg");

    let delivered = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let block = delivered.block.expect("terminal slots should render");
    assert!(block.contains("✓"), "rehydrated slots flipped ✓: {block}");
    assert!(
        delivered
            .delivered_terminal_ids
            .contains(&subagent_id("toolu_sa")),
        "subagent terminal id delivered: {:?}",
        delivered.delivered_terminal_ids
    );
    assert!(
        delivered
            .delivered_terminal_ids
            .contains(&bg_task_id("toolu_bg")),
        "bg task terminal id delivered: {:?}",
        delivered.delivered_terminal_ids
    );

    // #3391 eviction works on rehydrated slots: after delivered-once, they drop.
    events.evict_delivered_terminal_footer_tasks(channel_id, &delivered.delivered_terminal_ids);
    let after = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠼");
    assert!(
        after.block.is_none(),
        "both terminal slots evicted: {after:?}"
    );
}

#[test]
fn rehydration_is_idempotent() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_402_003);
    let transcript = write_transcript(&[
        transcript_subagent_start("toolu_sa", "one subagent"),
        transcript_background_bash_start("toolu_bg", "one bg"),
    ]);

    let first = events.rehydrate_slots_from_transcript_tail_for_footer_mode(
        channel_id,
        transcript.path(),
        true,
    );
    assert_eq!((first.subagents, first.background_tasks), (1, 1));
    // Re-running adds nothing (live slots already track both ids).
    let second = events.rehydrate_slots_from_transcript_tail_for_footer_mode(
        channel_id,
        transcript.path(),
        true,
    );
    assert_eq!(
        (second.subagents, second.background_tasks),
        (0, 0),
        "no duplicate restore"
    );

    let block = events
        .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
        .block
        .expect("slots should render");
    assert_eq!(
        block.matches("one subagent").count(),
        1,
        "single subagent slot: {block}"
    );
    assert_eq!(
        block.matches("one bg").count(),
        1,
        "single bg slot: {block}"
    );
}

#[test]
fn rehydration_bound_skips_records_before_compact_boundary() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_402_004);
    // The pre-compaction subagent must NOT be scanned; only the post-boundary one.
    let transcript = write_transcript(&[
        transcript_subagent_start("toolu_pre", "before compaction"),
        transcript_compact_boundary(),
        transcript_subagent_start("toolu_post", "after compaction"),
    ]);

    let outcome = events.rehydrate_slots_from_transcript_tail_for_footer_mode(
        channel_id,
        transcript.path(),
        true,
    );
    assert_eq!(
        outcome.subagents, 1,
        "only the post-boundary subagent restored"
    );

    let block = events
        .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
        .block
        .expect("post-boundary slot should render");
    assert!(
        block.contains("after compaction"),
        "post-boundary present: {block}"
    );
    assert!(
        !block.contains("before compaction"),
        "pre-boundary skipped: {block}"
    );
}

// ===========================================================================
// #3404: live (turn-in-progress) status-panel terminal-slot compaction.
// ===========================================================================

const LIVE_CAP: usize = super::completion_footer::LIVE_PANEL_TERMINAL_RENDER_CAP;

// #3404 fail-on-base: during a long turn the LIVE panel accumulated EVERY
// completed Task forever; the running entry and even the section budget got
// starved. Compaction keeps only the most recent `LIVE_CAP` completions plus all
// running entries and collapses the rest into a `… (+N completed)` summary. On
// HEAD (no compaction) all completed lines render and no summary line exists, so
// this fails.
#[test]
fn live_panel_compacts_completed_tasks_keeping_running_and_header() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_404_001);
    let completed = LIVE_CAP + 4;
    for i in 0..completed {
        let id = format!("toolu_3404_done_{i:02}");
        push_background_bash_task(&events, channel_id, &format!("Completed job {i:02}"), &id);
        complete_background_bash_task(&events, channel_id, &id);
    }
    push_background_bash_task(&events, channel_id, "Still running", "toolu_3404_run");

    let panel = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    let rendered_done = panel.matches('✓').count();
    assert_eq!(
        rendered_done, LIVE_CAP,
        "live panel must cap rendered completions at {LIVE_CAP}: {panel}"
    );
    let collapsed = completed - LIVE_CAP;
    assert!(
        panel.contains(&format!("… (+{collapsed} completed)")),
        "older completions must collapse into a summary: {panel}"
    );
    assert!(panel.contains("Tasks"), "Tasks header survives: {panel}");
    assert!(
        panel.contains("Still running"),
        "the running entry is never capped: {panel}"
    );
    // The most recent completion stays visible; the oldest is collapsed away.
    assert!(panel.contains(&format!("Completed job {:02}", completed - 1)));
    assert!(!panel.contains("Completed job 00"));
}

// #3404 fail-on-base: the bug's headline symptom — a long backlog of completed
// SUBAGENTS truncated the whole Subagents section to `…`. Compaction keeps the
// Subagents header + running entry visible. On HEAD the running subagent and the
// header are pushed out, so this fails.
#[test]
fn live_panel_compaction_keeps_subagents_section_and_running_entry() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_404_002);
    for i in 0..(LIVE_CAP + 5) {
        let id = format!("toolu_3404_sub_{i:02}");
        events.push_status_event(
            channel_id,
            StatusEvent::SubagentStart {
                subagent_type: Some("reviewer".to_string()),
                desc: Some(format!("Audit chunk {i:02}")),
                tool_use_id: Some(id.clone()),
                background: false,
            },
        );
        events.push_status_event(
            channel_id,
            StatusEvent::SubagentEnd {
                success: true,
                tool_use_id: Some(id),
                summary: None,
                ack_only: false,
            },
        );
    }
    events.push_status_event(
        channel_id,
        StatusEvent::SubagentStart {
            subagent_type: Some("reviewer".to_string()),
            desc: Some("Live inspection".to_string()),
            tool_use_id: Some("toolu_3404_sub_live".to_string()),
            background: false,
        },
    );

    let panel = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert!(
        panel.contains("Subagents"),
        "Subagents header survives compaction: {panel}"
    );
    assert!(
        panel.contains("Live inspection"),
        "the running subagent stays visible: {panel}"
    );
    assert_eq!(
        panel.matches('✓').count(),
        LIVE_CAP,
        "completed subagents are capped: {panel}"
    );
}

// #3404 SAFETY: the live path must NEVER mutate slot state — a ✓ that the live
// edit dropped from the RENDER must still be in state so the Ok-gated
// completion-footer eviction (#3391) remains authoritative and no ✓ is lost
// unseen. After a compacted live render, the completion footer still sees and
// can deliver every completed slot.
#[test]
fn live_panel_compaction_preserves_state_for_footer_eviction() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_404_003);
    let completed = LIVE_CAP + 3;
    let mut ids = Vec::new();
    for i in 0..completed {
        let id = format!("toolu_3404_state_{i:02}");
        push_background_bash_task(&events, channel_id, &format!("Job {i:02}"), &id);
        complete_background_bash_task(&events, channel_id, &id);
        ids.push(id);
    }

    // Live render compacts the display but must not touch state.
    let _ = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);

    // The completion footer (separate render) still reports EVERY completed slot
    // as deliverable — none were silently evicted by the live render.
    let footer = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    assert_eq!(
        footer.delivered_terminal_ids.len(),
        completed,
        "live compaction must not remove slots from state: {:?}",
        footer.delivered_terminal_ids
    );
}

// #3404: a small backlog at or under the cap renders verbatim (no summary line,
// no behavior change for short turns).
#[test]
fn live_panel_no_compaction_under_cap() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_404_004);
    for i in 0..LIVE_CAP {
        let id = format!("toolu_3404_small_{i:02}");
        push_background_bash_task(&events, channel_id, &format!("Job {i:02}"), &id);
        complete_background_bash_task(&events, channel_id, &id);
    }
    let panel = events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000);
    assert_eq!(panel.matches('✓').count(), LIVE_CAP);
    assert!(
        !panel.contains("completed)"),
        "no summary under cap: {panel}"
    );
}

// #3404 unit: the compaction primitive keeps the newest `cap` terminal lines,
// preserves every in-flight line in place, and emits one summary for the rest.
#[test]
fn compact_live_panel_terminal_lines_caps_and_preserves_inflight() {
    use super::completion_footer::compact_live_panel_terminal_lines;
    // Newest-first order (the live panel renders `.rev()`).
    let lines: Vec<String> = vec![
        "└ Bash running A ⠸".to_string(),
        "└ Bash done 5 ✓".to_string(),
        "└ Bash done 4 ✓".to_string(),
        "└ Bash done 3 ✓".to_string(),
        "└ Bash done 2 ✗".to_string(),
        "└ Bash done 1 ✓".to_string(),
    ];
    let (out, collapsed) =
        compact_live_panel_terminal_lines(&lines).expect("over-cap input must compact");
    assert_eq!(collapsed, 5 - LIVE_CAP);
    // Running line preserved in position.
    assert_eq!(out[0], "└ Bash running A ⠸");
    // Exactly `cap` terminal lines survive (the newest), plus one summary line.
    assert_eq!(
        out.iter()
            .filter(|l| l.ends_with('✓') || l.ends_with('✗'))
            .count(),
        LIVE_CAP
    );
    assert_eq!(out.iter().filter(|l| l.contains("completed)")).count(), 1);
    assert!(out.iter().any(|l| l.contains("done 5")));
    assert!(!out.iter().any(|l| l.contains("done 1")));
    // Under-cap input is left untouched.
    assert!(compact_live_panel_terminal_lines(&lines[..2].to_vec()).is_none());
}

// #3404 codex r1 — the compaction INFO log is count-change gated: same counts
// across render ticks must not re-log; zero counts re-arm the gate so the next
// compaction episode logs again.
#[test]
fn compaction_log_gate_fires_on_change_only() {
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_404_100);
    assert!(
        events.compaction_counts_changed(channel_id, 4, 0),
        "first over-cap render logs"
    );
    assert!(
        !events.compaction_counts_changed(channel_id, 4, 0),
        "same counts stay silent"
    );
    assert!(
        events.compaction_counts_changed(channel_id, 5, 1),
        "count growth logs"
    );
    assert!(
        !events.compaction_counts_changed(channel_id, 5, 1),
        "steady state stays silent"
    );
    assert!(
        !events.compaction_counts_changed(channel_id, 0, 0),
        "zero never logs"
    );
    assert!(
        events.compaction_counts_changed(channel_id, 5, 1),
        "zero re-arms the next episode"
    );
    // codex r2: a turn reset (even residual-preserving) re-arms the gate.
    events.clear_channel_preserving_footer_residuals(channel_id);
    assert!(
        events.compaction_counts_changed(channel_id, 5, 1),
        "turn reset re-arms the gate without an interleaved zero render"
    );
}

// ===========================================================================
// #3477 / #3473 — live panel terminal block reorder/blank/multiline + TTL.
// ===========================================================================

// #3477 item 1: multi-line tool output stays readable in the Recent/terminal
// block (multiple lines preserved) instead of collapsing to one run-on line.
#[test]
fn recent_block_preserves_multiline_tool_output() {
    let multiline =
        "error[E0308]: mismatched types\n  expected `u64`, found `i64`\n  at src/main.rs:10";
    let event = RecentPlaceholderEvent::tool_error(multiline).expect("event");
    let rendered = event.render_line();
    let line_count = rendered.lines().count();
    assert!(
        line_count >= 2,
        "multi-line output must keep multiple lines, got {line_count}: {rendered:?}"
    );
    assert!(
        rendered.contains("E0308"),
        "first line preserved: {rendered:?}"
    );
    assert!(
        rendered.contains("expected"),
        "continuation line preserved: {rendered:?}"
    );
}

// #3477 item 1: the compact single-line panel cells (Tasks/Subagents) stay
// one-line — `normalize_summary` (first line only) is unchanged.
#[test]
fn normalize_summary_stays_single_line_for_panel_cells() {
    let collapsed = super::common::normalize_summary("first line\nsecond line\nthird");
    assert!(
        !collapsed.contains('\n'),
        "panel cell must stay single-line: {collapsed:?}"
    );
    assert_eq!(collapsed, "first line");
}

// #3477 item 1: the task-card summary preserves newlines (Discord renders
// multi-line bold), while still neutralizing the ``` fence hazard.
#[test]
fn task_card_summary_preserves_newlines() {
    let card = super::super::tui_task_card::format_task_notification_card(
        &super::super::tui_task_card::TaskNotification {
            status: Some("completed".to_string()),
            summary: Some("line one\nline two\n```danger```".to_string()),
            ..Default::default()
        },
        1,
    );
    assert!(
        card.contains("line one\nline two"),
        "newlines preserved: {card}"
    );
    assert!(
        !card.contains("```danger```"),
        "fence hazard escaped: {card}"
    );
}

// #3473: a background task slot stuck past the TTL is force-aborted at the turn
// boundary so it renders ✗ and is evicted (dropped) — it no longer sits ⏳
// forever; a fresh slot in the same turn is untouched (normal completion path).
#[test]
fn stuck_background_task_slot_force_aborted_at_turn_boundary() {
    use super::task_panel::{STUCK_BACKGROUND_TASK_TTL, force_abort_stuck_background_task_slots};

    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_473_001);
    // A stuck slot whose terminal notification never arrives.
    events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskStart {
            name: "Bash".to_string(),
            summary: "stuck job".to_string(),
            tool_use_id: "stuck-1".to_string(),
        },
    );
    // A second, fresh background slot started "now".
    events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskStart {
            name: "Bash".to_string(),
            summary: "fresh job".to_string(),
            tool_use_id: "fresh-1".to_string(),
        },
    );
    // Back-date the first slot's creation past the TTL.
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stale_at = std::time::Instant::now()
            .checked_sub(STUCK_BACKGROUND_TASK_TTL + std::time::Duration::from_secs(60))
            .expect("monotonic clock far enough past origin");
        let stuck = guard
            .tasks
            .iter_mut()
            .find(|slot| slot.tool_use_id.as_deref() == Some("stuck-1"))
            .expect("stuck slot");
        stuck.created_at = stale_at;
        // The direct helper aborts exactly the stale slot, not the fresh one.
        let aborted =
            force_abort_stuck_background_task_slots(&mut guard.tasks, std::time::Instant::now());
        assert_eq!(aborted, 1, "only the stale slot is aborted");
        assert_eq!(
            guard
                .tasks
                .iter()
                .find(|slot| slot.tool_use_id.as_deref() == Some("stuck-1"))
                .and_then(|slot| slot.status.as_deref()),
            Some("aborted")
        );
        assert!(
            guard
                .tasks
                .iter()
                .find(|slot| slot.tool_use_id.as_deref() == Some("fresh-1"))
                .map(|slot| slot.status.is_none())
                .unwrap_or(false),
            "fresh slot must stay in progress"
        );
    }
}

// #3473: the turn-boundary reconciliation (the production call site) drops the
// stuck slot — it is no longer retained as an unfinished-background residual —
// while a fresh background slot survives as a residual.
#[test]
fn stuck_background_task_slot_dropped_on_turn_boundary_reconciliation() {
    use super::task_panel::STUCK_BACKGROUND_TASK_TTL;

    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3_473_002);
    events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskStart {
            name: "Bash".to_string(),
            summary: "stuck job".to_string(),
            tool_use_id: "stuck-2".to_string(),
        },
    );
    events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskStart {
            name: "Bash".to_string(),
            summary: "fresh job".to_string(),
            tool_use_id: "fresh-2".to_string(),
        },
    );
    {
        let entry = events
            .status_by_channel
            .get(&channel_id)
            .expect("status panel state");
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stale_at = std::time::Instant::now()
            .checked_sub(STUCK_BACKGROUND_TASK_TTL + std::time::Duration::from_secs(60))
            .expect("monotonic clock far enough past origin");
        guard
            .tasks
            .iter_mut()
            .find(|slot| slot.tool_use_id.as_deref() == Some("stuck-2"))
            .expect("stuck slot")
            .created_at = stale_at;
    }

    // Turn boundary: the production reconciliation site.
    events.clear_channel_preserving_footer_residuals(channel_id);

    let entry = events
        .status_by_channel
        .get(&channel_id)
        .expect("residual state survives because the fresh slot is preserved");
    let guard = entry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        guard
            .tasks
            .iter()
            .all(|slot| slot.tool_use_id.as_deref() != Some("stuck-2")),
        "stuck slot must be dropped at the turn boundary: {:?}",
        guard.tasks
    );
    assert!(
        guard
            .tasks
            .iter()
            .any(|slot| slot.tool_use_id.as_deref() == Some("fresh-2")),
        "fresh background slot must survive as a residual: {:?}",
        guard.tasks
    );
}

// ===========================================================================
// #3811: deterministic turn anchors on result/status surfaces.
//
// The pure `render_request_anchor_line` gating (real-id/guild → link;
// headless/synthetic/voice/0 → no link) is unit-tested inline in
// `turn_anchor.rs`. These tests cover the two render surfaces (target tags now
// on the completion footer, 요청 line prepended first + surviving overflow) and
// the snapshot lifecycle (preserve-across-turn-reset, clear-on-TUI-direct,
// clear-on-session-reset). The store-level `render_*` wrappers read the guild id
// from `load_graceful()`, which is config-dependent in tests, so the request
// LINK rendering is asserted via the free renderers with an explicit anchor line.
// ===========================================================================

// A real Discord snowflake (well below the 8e18 synthetic floor).
const ANCHOR_TEST_USER_MSG_ID: u64 = 1_520_312_799_245_504_542;

#[test]
fn completion_footer_renders_target_tags_for_dispatch_linked_turn() {
    // #3811: the result/final surface previously carried NEITHER the request link
    // NOR the target tags. It must now render the 대상 tags from the existing task
    // snapshot even with no Tasks/Subagents content.
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(3811);
    assert!(events.set_task_panel_info(
        channel_id,
        TaskPanelInfo {
            dispatch_id: "bddc480d-43d1-4f1f-b3fd-e0d96b3b3d82",
            dispatch_type: Some("review"),
            card_title: Some("Fix CI inventory drift"),
            github_issue_number: Some(3805),
            ..Default::default()
        },
    ));

    let rendered = events.render_completion_footer(channel_id, &ProviderKind::Claude, "⠸");
    let block = rendered
        .block
        .expect("dispatch-linked footer should render the target tags");
    assert!(block.contains("gh#3805"), "missing issue tag: {block:?}");
    assert!(
        block.contains("dsp #bddc480d"),
        "missing dispatch tag: {block:?}"
    );
}

#[test]
fn completion_footer_free_renderer_prepends_request_anchor_and_target() {
    // Anchor leads, then the 대상 target tags — both on the result surface. The
    // snapshot is built through the store (its fields are module-private) and
    // cloned out so the free renderer can be exercised with an explicit anchor.
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(38114);
    assert!(events.set_task_panel_info(
        channel_id,
        TaskPanelInfo {
            dispatch_id: "d_abc12345",
            dispatch_type: Some("review"),
            card_title: Some("Fix CI inventory drift"),
            github_issue_number: Some(3805),
            ..Default::default()
        },
    ));
    let snapshot = events
        .status_by_channel
        .get(&channel_id)
        .expect("status panel state")
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let render = super::completion_footer::render_completion_footer(
        snapshot,
        &ProviderKind::Claude,
        "⠸",
        Some("턴 트리거: https://discord.com/channels/1/2/3".to_string()),
    );
    let block = render
        .block
        .expect("anchor + target should render on the result surface");
    assert!(
        block.starts_with("턴 트리거: https://discord.com/channels/1/2/3"),
        "request anchor must lead the footer: {block:?}"
    );
    assert!(
        block.contains("gh#3805"),
        "missing target issue tag: {block:?}"
    );
    assert!(
        block.contains("dsp #d"),
        "missing target dispatch tag: {block:?}"
    );
}

#[test]
fn completion_footer_free_renderer_omits_anchor_and_target_when_absent() {
    // Missing metadata → no block at all (omitted fields, not placeholder noise).
    let render = super::completion_footer::render_completion_footer(
        StatusPanelState::default(),
        &ProviderKind::Claude,
        "⠸",
        None,
    );
    assert!(
        render.block.is_none(),
        "absent anchor/target/content must yield no footer noise: {:?}",
        render.block
    );
}

#[test]
fn status_panel_free_renderer_leads_with_activity_and_time_lines() {
    // #3983: the panel opens with the activity label (line 1) then the time line
    // (line 2); the 턴 트리거 deeplink trails as the last section.
    let out = super::status_panel::render_status_panel(
        StatusPanelState::default(),
        &ProviderKind::Claude,
        "마지막 업데이트 : <t:1700000000:R> / 턴 시작 : <t:1700000000:R>".to_string(),
        Some("턴 트리거: https://discord.com/channels/1/2/3".to_string()),
    );
    let mut sections = out.split("\n\n");
    assert_eq!(
        sections.next(),
        Some("🟢 진행 중"),
        "line 1 = activity: {out:?}"
    );
    assert_eq!(
        sections.next(),
        Some("마지막 업데이트 : <t:1700000000:R> / 턴 시작 : <t:1700000000:R>"),
        "line 2 = time line: {out:?}"
    );
    assert!(
        out.trim_end()
            .ends_with("턴 트리거: https://discord.com/channels/1/2/3"),
        "턴 트리거 must be the last footer line: {out:?}"
    );
    assert!(
        out.chars().count() <= STATUS_PANEL_MAX_CHARS,
        "panel must respect the size cap"
    );
}

#[test]
fn turn_request_anchor_survives_turn_reset() {
    // #3811 lifecycle: an intake-set anchor must survive the bridge's same-turn
    // reset (no footer residuals), otherwise the entry+anchor would be dropped
    // before the turn renders its request link.
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(38111);
    events.set_turn_request_anchor(channel_id, Some(ANCHOR_TEST_USER_MSG_ID));
    events.clear_channel_preserving_footer_residuals(channel_id);
    assert_eq!(
        events.request_user_msg_id_for_test(channel_id),
        Some(ANCHOR_TEST_USER_MSG_ID),
        "anchor must be preserved across the turn-content reset"
    );
}

#[test]
fn turn_request_anchor_not_bled_by_queued_message_before_promotion() {
    // #3811 P1 regression (codex review): the intake setter is gated on
    // `started == true` (the mailbox claim was WON). A message that merely QUEUES
    // behind an active turn issues NO setter call, so it cannot overwrite the
    // active turn's deeplink; it records its own anchor only when later
    // dequeued/promoted (re-entering intake with `started == true`). This pins the
    // store-side contract that gating relies on: the anchor changes ONLY on an
    // explicit setter call, and survives the active turn's same-turn bridge reset.
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(38115);
    let turn_a_msg = 1_520_000_000_000_000_001u64;
    let turn_b_msg = 1_520_000_000_000_000_777u64;

    // Turn A wins the claim (started == true) and records its anchor; the bridge
    // then runs A's same-turn reset, which preserves it.
    events.set_turn_request_anchor(channel_id, Some(turn_a_msg));
    events.clear_channel_preserving_footer_residuals(channel_id);
    assert_eq!(
        events.request_user_msg_id_for_test(channel_id),
        Some(turn_a_msg)
    );

    // Message B arrives while A is active and only QUEUES (started == false):
    // intake issues NO setter call, so A's anchor stays put (no cross-turn bleed).
    assert_eq!(
        events.request_user_msg_id_for_test(channel_id),
        Some(turn_a_msg),
        "a queued message must not bleed the active turn's anchor"
    );

    // B is later dequeued/promoted (started == true) and records its own anchor.
    events.set_turn_request_anchor(channel_id, Some(turn_b_msg));
    assert_eq!(
        events.request_user_msg_id_for_test(channel_id),
        Some(turn_b_msg),
        "promotion updates the anchor to the now-active turn"
    );
}

#[test]
fn turn_request_anchor_cleared_on_tui_direct() {
    // The TUI-direct path passes `None` so a prior interactive link can never leak
    // onto a later id-0 synthetic turn.
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(38112);
    events.set_turn_request_anchor(channel_id, Some(ANCHOR_TEST_USER_MSG_ID));
    events.set_turn_request_anchor(channel_id, None);
    assert_eq!(events.request_user_msg_id_for_test(channel_id), None);
}

#[test]
fn turn_request_anchor_cleared_on_session_reset() {
    // A genuine provider-session boundary is a new request context → drop the
    // prior turn's anchor.
    let events = PlaceholderLiveEvents::default();
    let channel_id = ChannelId::new(38113);
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch38113#100"),
        "session_resumed",
        &json!({ "provider_session_id": "session-A", "tmux_reused": true }),
    ));
    events.set_turn_request_anchor(channel_id, Some(ANCHOR_TEST_USER_MSG_ID));
    assert_eq!(
        events.request_user_msg_id_for_test(channel_id),
        Some(ANCHOR_TEST_USER_MSG_ID)
    );
    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        Some("AgentDesk-claude-ch38113#200"),
        "session_fresh",
        &json!({ "provider_session_id": "session-B", "tmux_reused": false }),
    ));
    assert_eq!(
        events.request_user_msg_id_for_test(channel_id),
        None,
        "anchor must be cleared on the session boundary"
    );
}
