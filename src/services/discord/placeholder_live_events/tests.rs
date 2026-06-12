use super::super::formatting::{
    MonitorHandoffReason, MonitorHandoffStatus, build_monitor_handoff_placeholder_with_live_events,
    build_processing_status_block, build_streaming_placeholder_text, plan_streaming_rollover,
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
    assert!(panel.contains("cargo test --lib placeholder_live_events"));

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
        None,
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

    let fresh = events.render_status_panel(fresh_channel_id, &ProviderKind::Codex, 1_700_000_000);
    assert!(fresh.contains("🆕 새 세션 시작"));
    assert!(fresh.contains("provider session codex#fresh-se…"));
    assert!(fresh.contains("tmux new"));

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
        None,
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
        None,
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
        None,
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
        None,
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
        None,
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

    assert!(events.set_session_panel_lifecycle_event(
        channel_id,
        None,
        "session_resumed",
        &json!({}),
    ));

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
// id as a top-level `parent_tool_use_id`. Its tool step must surface on the
// owning subagent slot (`└ type desc — [Tool] args`), not the panel header, so a
// long (background) subagent is not an opaque "running".
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
        rendered.contains("[Bash]") && rendered.contains("grep ERROR app.log"),
        "subagent activity line missing, got: {rendered}"
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
