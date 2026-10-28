use super::super::status_events::{
    status_events_from_json, status_events_from_tool_result_with_id,
    status_events_from_tool_use_with_id,
};
use super::*;
use serde_json::json;

fn start(ring: &mut ToolActivityRing, name: &str, id: &str) {
    for event in
        status_events_from_tool_use_with_id(name, r#"{"description":"audit source"}"#, Some(id))
    {
        ring.apply(&event, None);
    }
}

#[test]
fn accuracy_unknown_result_preserves_pending_and_completed_rows() {
    let mut ring = ToolActivityRing::default();
    start(&mut ring, "Read", "done");
    ring.apply(&observation(None, None, Some("done"), Some(true)), None);
    start(&mut ring, "Read", "active");
    for event in status_events_from_json(
        &json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"not-in-ring","content":"PRIVATE_RESULT"}]}}),
    ) {
        ring.apply(&event, None);
    }
    let out = ring.render(6);
    assert!(
        out.contains("실행 1") && out.contains("▶ Read") && out.contains("✓ Read"),
        "{out}"
    );
    assert!(
        !out.contains('?') && !out.contains("PRIVATE_RESULT"),
        "{out}"
    );
    ring.apply(&observation(None, None, Some("active"), Some(false)), None);
    assert!(ring.render(6).contains("실패 1"));
    assert_eq!(
        ring.rows.iter().filter(|r| r.result == Some(true)).count(),
        1
    );
}

#[test]
fn accuracy_spawn_ack_does_not_finish_the_agent() {
    let mut ring = ToolActivityRing::default();
    start(&mut ring, "functions.spawn_agent", "spawn-call");
    for event in status_events_from_tool_result_with_id(
        Some("functions.spawn_agent"),
        false,
        Some("spawn-call"),
    ) {
        ring.apply(&event, None);
    }
    let out = ring.render(6);
    assert!(
        out.contains("↗ Task") && out.contains("시작됨") && out.contains("실행 0"),
        "{out}"
    );
    assert!(
        !out.contains('✓') && !out.contains("▶ Task") && !out.contains("시작 <t:"),
        "{out}"
    );
    start(&mut ring, "spawn_agent", "failed-launch");
    ring.apply(
        &observation(None, None, Some("failed-launch"), Some(false)),
        None,
    );
    assert!(ring.render(6).contains("✗ Task"));
}

#[test]
fn accuracy_codex_spawn_wait_rollout_keeps_launch_ack_out_of_running_count() {
    let mut ring = ToolActivityRing::default();
    let mut calls = std::collections::BTreeMap::new();
    for value in [
        json!({"type":"response_item","payload":{"type":"function_call","name":"spawn_agent","call_id":"spawn-1","arguments":"{\"message\":\"PRIVATE_PROMPT\"}"}}),
        json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"spawn-1","output":"{\"agent_id\":\"agent-1\"}"}}),
        json!({"type":"response_item","payload":{"type":"function_call","name":"wait_agent","call_id":"wait-1","arguments":"{\"ids\":[\"agent-1\"]}"}}),
        json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"wait-1","output":"{\"status\":{\"agent-1\":{\"completed\":\"PRIVATE_RESULT\"}},\"timed_out\":false}"}}),
    ] {
        let payload = &value["payload"];
        let id = payload["call_id"].as_str().unwrap();
        let events = if payload["type"] == "function_call" {
            let name = payload["name"].as_str().unwrap();
            calls.insert(id.to_string(), name.to_string());
            status_events_from_tool_use_with_id(
                name,
                payload["arguments"].as_str().unwrap(),
                Some(id),
            )
        } else {
            status_events_from_tool_result_with_id(
                calls.get(id).map(String::as_str),
                false,
                Some(id),
            )
        };
        for event in events {
            ring.apply(&event, None);
        }
    }
    let out = ring.render(6);
    assert!(
        out.contains("↗ Task") && out.contains("시작됨") && out.contains("실행 0"),
        "{out}"
    );
    assert!(
        !out.contains("▶ Task") && !out.contains("✓ Task") && !out.contains("PRIVATE_"),
        "{out}"
    );
    ring.apply(
        &StatusEvent::TurnCompleted {
            background: false,
            background_agent_pending: false,
        },
        None,
    );
    assert!(ring.render(6).contains("↗ Task"));
    start(&mut ring, "Read", "active-read");
    for index in 0..7 {
        let id = format!("spawn-{index}-later");
        start(&mut ring, "spawn_agent", &id);
        ring.apply(&observation(None, None, Some(&id), Some(true)), None);
    }
    let out = ring.render(6);
    assert!(out.contains("▶ Read") && out.contains("실행 1"), "{out}");
}

#[test]
fn accuracy_claude_background_task_closes_on_task_notification() {
    let mut ring = ToolActivityRing::default();
    for event in status_events_from_tool_use_with_id(
        "Task",
        r#"{"description":"audit source","run_in_background":true}"#,
        Some("claude-task"),
    ) {
        ring.apply(&event, None);
    }
    for event in status_events_from_tool_result_with_id(Some("Task"), false, Some("claude-task")) {
        ring.apply(&event, None);
    }
    let out = ring.render(6);
    assert!(
        out.contains("▶ Task") && out.contains("실행 1") && !out.contains("↗"),
        "{out}"
    );
    for event in super::super::status_events::status_events_from_task_notification_with_tool_use_id(
        "subagent",
        "completed",
        "audit finished",
        Some("claude-task"),
    ) {
        ring.apply(&event, None);
    }
    let out = ring.render(6);
    assert!(
        out.contains("✓ Task") && out.contains("실행 0") && !out.contains("▶ Task"),
        "{out}"
    );
}

#[test]
fn accuracy_panel_budget_keeps_plan_tasks_and_subagents_sections() {
    let sections = vec![
        "🔧 도구 1 · 실행 1 · 실패 0\n▶ Bash cargo check".into(),
        format!("Plan\n{}", "- [ ] 긴 계획\n".repeat(90)),
        format!("Tasks\n{}", "- 작업 진행\n".repeat(90)),
        format!("Subagents\n{}", "- 에이전트 진행\n".repeat(90)),
    ];
    let out = fit_panel(sections);
    for title in ["Plan", "Tasks", "Subagents"] {
        assert!(out.contains(title), "{title}: {out}");
    }
    assert!(
        out.len() <= 1400 && out.encode_utf16().count() <= 1936,
        "{out}"
    );
}

#[test]
fn accuracy_original_timestamp_survives_replay_and_sixty_second_clock_shift() {
    let _clock = super::super::test_clock::Guard::new();
    let value = json!({"type":"assistant", "timestamp":"2026-10-18T10:00:00Z", "message":{"content":[{"type":"tool_use","id":"stamp","name":"Bash","input":{"command":"cargo check"}}]}});
    let observe = || {
        let mut ring = ToolActivityRing::default();
        for event in status_events_from_json(&value) {
            ring.apply(&event, None);
        }
        ring.render(6)
    };
    let first = observe();
    assert!(first.contains("<t:1792317600:R>"), "{first}");
    let mut state = super::super::status_panel::StatusPanelState::default();
    super::super::test_clock::aged_stamp(&mut state, std::time::Duration::from_secs(60));
    assert_eq!(first, observe());
}

#[test]
fn accuracy_timestamp_defaults_clamps_and_enriches_partial_starts() {
    let _clock = super::super::test_clock::Guard::new();
    let old: StatusEvent = serde_json::from_value(
        json!({"ToolActivity":{"name":"Read", "input":null, "tool_use_id":"old", "success":null}}),
    )
    .unwrap();
    assert!(matches!(
        old,
        StatusEvent::ToolActivity {
            source_unix: None,
            ..
        }
    ));
    for stamp in [
        Value::Null,
        json!(0),
        json!(-1),
        json!("invalid"),
        json!(chrono::Utc::now().timestamp() + 3600),
    ] {
        let mut value = json!({"type":"assistant", "message":{"content":[{"type":"tool_use","id":"time","name":"Read","input":{"path":"src/lib.rs"}}]}});
        if !stamp.is_null() {
            value["timestamp"] = stamp;
        }
        let before = observed_unix();
        let mut ring = ToolActivityRing::default();
        for event in status_events_from_json(&value) {
            ring.apply(&event, None);
        }
        assert_eq!(ring.rows.len(), 1);
        let row = ring.rows.front().unwrap();
        assert!(
            row.started_unix >= before && row.started_unix <= observed_unix(),
            "{value}: {}",
            row.started_unix
        );
    }
    let mut ring = ToolActivityRing::default();
    start(&mut ring, "Read", "partial");
    for event in status_events_from_json(
        &json!({"type":"assistant", "timestamp":1792317600,"message":{"content":[{"type":"tool_use","id":"partial","name":"Read","input":{"path":"src/lib.rs"}}]}}),
    ) {
        ring.apply(&event, None);
    }
    assert_eq!(ring.rows.len(), 1);
    assert_eq!(ring.rows[0].started_unix, 1792317600);
}

#[test]
fn accuracy_panel_keeps_safe_headings_when_content_must_be_hidden() {
    let out = fit_panel(vec![
        "🔧 도구 1".into(),
        "Plan\n- [ ] env MYSQL PrivateSentinel".into(),
        "Tasks\n-----BEGIN PRIVATE KEY-----\nSmallKeyMaterial".into(),
    ]);
    assert!(out.contains("Plan") && out.contains("Tasks"), "{out}");
    assert!(
        !out.contains("PrivateSentinel") && !out.contains("SmallKeyMaterial"),
        "{out}"
    );
}

#[test]
fn accuracy_numeric_record_times_are_seconds_and_settle_elapsed_from_record_times() {
    let _clock = super::super::test_clock::Guard::new();
    let tool = |stamp: Value| json!({"type":"assistant","timestamp":stamp,"message":{"content":[{"type":"tool_use","id":"bash","name":"Bash","input":{"command":"cargo check"}}]}});
    let result = |stamp: Value| json!({"type":"user","timestamp":stamp,"message":{"content":[{"type":"tool_result","tool_use_id":"bash","content":"ok"}]}});
    let replay = |records: &[Value]| {
        let mut ring = ToolActivityRing::default();
        for record in records {
            for event in status_events_from_json(record) {
                ring.apply(&event, None);
            }
        }
        ring
    };
    let ms = replay(&[tool(json!(1_792_317_600_000_i64))]);
    assert!(
        ms.render(6).contains("<t:1792317600:R>"),
        "{}",
        ms.render(6)
    );
    assert_eq!(
        replay(&[tool(json!(1_600_000_000))]).rows[0].started_unix,
        1_600_000_000
    );
    for implausible in [
        json!(1_599_999_999),
        json!(1_599_999_999_999_i64),
        json!(1_000_000),
    ] {
        let before = observed_unix();
        let ring = replay(&[tool(implausible.clone())]);
        assert!(ring.rows[0].started_unix >= before, "{implausible}");
    }
    for (start, end) in [
        (json!("2026-10-18T10:00:00Z"), json!("2026-10-18T10:00:42Z")),
        (json!(1_792_317_600_000_i64), json!(1_792_317_642_000_i64)),
    ] {
        let out = replay(&[tool(start), result(end)]).render(6);
        assert!(out.contains("✓ Bash") && out.contains("42초"), "{out}");
    }
    let one_sided = replay(&[tool(Value::Null), result(json!("2026-10-18T10:00:42Z"))]);
    assert!(
        one_sided.render(6).contains("0초"),
        "{}",
        one_sided.render(6)
    );
}
