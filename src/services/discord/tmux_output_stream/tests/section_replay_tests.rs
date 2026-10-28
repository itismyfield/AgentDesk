//! Replays provider-shaped records through the watcher parser into the live status panel
//! and footer, so Plan/Tasks/Subagents visibility is checked per tool-activity display mode.
use super::*;
use crate::config::ToolActivityDisplay;
use crate::services::discord::placeholder_live_events::PlaceholderLiveEvents;
use serde_json::{Value, json};

const STARTED: i64 = 1_792_000_000;

fn replay(
    provider: ProviderKind,
    records: &[Value],
    mode: ToolActivityDisplay,
    body: &str,
) -> [String; 2] {
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    let ui = &mut Arc::get_mut(&mut shared).expect("unshared fixture").ui;
    ui.status_panel_v2_enabled = true;
    ui.placeholder_live_events = Arc::new(PlaceholderLiveEvents::with_tool_activity_display(mode));
    let channel = ChannelId::new(7132);
    let mut buffer: String = records.iter().map(|record| format!("{record}\n")).collect();
    let mut tools = WatcherToolState::new();
    tools.set_provider(&provider);
    process_watcher_lines(
        &mut buffer,
        &mut StreamLineState::new(),
        &mut String::new(),
        &mut tools,
    );
    assert!(buffer.is_empty() && flush_placeholder_live_events(&shared, channel, &mut tools));
    let live = &shared.ui.placeholder_live_events;
    [
        live.render_status_panel(channel, &provider, STARTED),
        live.render_footer_status_panel(channel, &provider, STARTED, (body, "⠋")),
    ]
}

fn titles(text: &str) -> Vec<&str> {
    text.lines()
        .map(|line| line.trim().trim_start_matches("-# "))
        .filter(|line| ["Plan", "Tasks", "Subagents"].contains(line))
        .collect()
}

fn claude_tool(id: &str, name: &str, input: Value) -> [Value; 1] {
    [
        json!({"type":"assistant","timestamp":"2026-10-27T07:00:00Z","cwd":"/tmp/fixture",
        "message":{"role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":input}]}}),
    ]
}

fn claude_result(id: &str, extra: Value) -> Value {
    let mut record = json!({"type":"user","timestamp":"2026-10-27T07:00:01Z",
        "message":{"role":"user","content":[{"tool_use_id":id,"type":"tool_result",
            "content":[{"type":"text","text":"더미 결과"}]}]}});
    if !extra.is_null() {
        record["toolUseResult"] = extra;
    }
    record
}

fn claude_records(commands: usize, plan_text: &str, agent_text: &str, agents: usize) -> Vec<Value> {
    let todos: Vec<_> = (0..8)
        .map(|i| json!({"content":format!("{plan_text} {i}"),"status":if i == 0 {"in_progress"} else {"pending"},"activeForm":"더미 진행"}))
        .collect();
    let mut records = Vec::new();
    records.extend(claude_tool(
        "toolu_plan",
        "TodoWrite",
        json!({"todos":todos}),
    ));
    records.push(claude_result("toolu_plan", Value::Null));
    records.extend(claude_tool(
        "toolu_task",
        "TaskCreate",
        json!({"subject":"더미 작업","description":"더미 설명","activeForm":"더미 작업 중"}),
    ));
    records.push(claude_result(
        "toolu_task",
        json!({"task":{"id":"1","subject":"더미 작업"}}),
    ));
    for i in 0..agents {
        let (id, desc) = (format!("toolu_agent{i}"), format!("{agent_text} {i}"));
        records.extend(claude_tool(
            &id,
            "Agent",
            json!({"description":desc,"subagent_type":"Explore","prompt":"더미 프롬프트","run_in_background":true}),
        ));
        records.push(claude_result(
            &id,
            json!({"isAsync":true,"status":"async_launched","agentId":format!("a{i}fixture"),"description":desc}),
        ));
    }
    for i in 0..commands {
        let id = format!("toolu_bash{i}");
        records.extend(claude_tool(
            &id,
            "Bash",
            json!({"command":format!("cargo check -p fixture{i} {}", "x".repeat(60)),"description":format!("더미 빌드 확인 {i} {}", "가".repeat(40))}),
        ));
        if i + 1 < commands {
            records.push(claude_result(&id, Value::Null));
        }
    }
    records
}

fn codex_records(commands: usize) -> Vec<Value> {
    let item = |payload: Value| json!({"timestamp":"2026-10-27T07:00:00.000Z","type":"response_item","payload":payload});
    let mut records = vec![
        json!({"timestamp":"2026-10-27T07:00:00.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-fixture"}}),
        item(
            json!({"type":"function_call","name":"update_plan","call_id":"call_plan",
            "arguments":"{\"plan\":[{\"step\":\"더미 계획 하나\",\"status\":\"in_progress\"},{\"step\":\"더미 계획 둘\",\"status\":\"pending\"}]}"}),
        ),
        item(json!({"type":"function_call_output","call_id":"call_plan","output":"Plan updated"})),
        item(
            json!({"type":"function_call","name":"spawn_agent","namespace":"collaboration","call_id":"call_spawn",
            "arguments":"{\"task_name\":\"dummy_agent\",\"message\":\"더미 지시\"}"}),
        ),
        item(
            json!({"type":"function_call_output","call_id":"call_spawn","output":"{\"task_name\":\"/root/dummy_agent\"}"}),
        ),
    ];
    for i in 0..commands {
        let id = format!("call_exec{i}");
        records.push(item(json!({"type":"custom_tool_call","status":"completed","name":"exec","call_id":id,
            "input":format!("const r = await tools.exec_command({{cmd: \"cargo check -p fixture{i}\"}});\ntext(r.output);")})));
        if i + 1 < commands {
            records.push(item(json!({"type":"custom_tool_call_output","call_id":id,
                "output":[{"type":"input_text","text":"Script completed\nWall time 0.1 seconds\nOutput:"},{"type":"input_text","text":"더미 출력"}]})));
        }
    }
    records
}

// An empty body, a short body, and one long enough that the footer is admitted as class.
fn bodies() -> [String; 3] {
    [
        String::new(),
        "짧은 응답".into(),
        "긴 응답 본문 ".repeat(300),
    ]
}

#[test]
fn section_replay_claude_footer_keeps_plan_tasks_subagents_in_every_admission() {
    // The ASCII case exceeds the 2000-unit panel limit yet fits 1400 bytes once Subagents is
    // dropped whole, so only a lossless budget check keeps that section in the footer.
    let ascii = (
        "p".repeat(100),
        format!("dummy research {}", "y".repeat(90)),
    );
    for (records, pressure) in [
        (claude_records(2, "더미 계획", "더미 조사", 1), "none"),
        (
            claude_records(12, &"긴 더미 계획 항목".repeat(10), "더미 조사", 1),
            "korean",
        ),
        (claude_records(2, &ascii.0, &ascii.1, 10), "ascii"),
    ] {
        for mode in [ToolActivityDisplay::Summary, ToolActivityDisplay::Class] {
            for body in bodies() {
                let [panel, footer] = replay(ProviderKind::Claude, &records, mode, &body);
                let case = format!("{mode:?} pressure={pressure} body={}", body.len());
                assert!(panel.encode_utf16().count() <= 2000, "{case}");
                for text in [&panel, &footer] {
                    assert_eq!(
                        titles(text),
                        ["Plan", "Tasks", "Subagents"],
                        "{case}\n{text}"
                    );
                }
                let first = footer.lines().next().unwrap_or_default();
                let summary_footer = mode == ToolActivityDisplay::Summary && body.len() < 1000;
                assert_eq!(
                    first.contains("· 실행 "),
                    summary_footer,
                    "{case}\n{footer}"
                );
                assert!(footer.encode_utf16().count() <= 2000, "{case}");
            }
        }
    }
}

#[test]
fn section_replay_codex_footer_keeps_plan_and_hides_subagents_in_every_admission() {
    let records = codex_records(2);
    for mode in [ToolActivityDisplay::Summary, ToolActivityDisplay::Class] {
        for body in bodies() {
            let [panel, footer] = replay(ProviderKind::Codex, &records, mode, &body);
            for text in [&panel, &footer] {
                assert_eq!(
                    titles(text),
                    ["Plan"],
                    "{mode:?} body={}\n{text}",
                    body.len()
                );
            }
            if mode == ToolActivityDisplay::Summary {
                assert!(
                    panel.contains("↗ Task") && panel.contains("✓ update\\_plan"),
                    "{panel}"
                );
            }
        }
    }
}

#[test]
fn section_replay_codex_spawn_rows_settle_from_wait_and_close_results() {
    let item = |payload: Value| json!({"timestamp":"2026-10-27T07:00:00.000Z","type":"response_item","payload":payload});
    let call = |name: &str, id: &str, arguments: Value| {
        item(
            json!({"type":"function_call","name":name,"namespace":"collaboration","call_id":id,"arguments":arguments.to_string()}),
        )
    };
    let output = |id: &str, output: Value| {
        item(json!({"type":"function_call_output","call_id":id,"output":output.to_string()}))
    };
    let mut records = vec![
        json!({"timestamp":"2026-10-27T07:00:00.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-fixture"}}),
    ];
    for agent in ["done", "failed", "closed", "running"] {
        let id = format!("call_spawn_{agent}");
        records.push(call("spawn_agent", &id, json!({"message":"더미 지시"})));
        records.push(output(
            &id,
            json!({"agent_id":format!("agent-{agent}"),"nickname":null}),
        ));
    }
    records.push(call(
        "wait_agent",
        "call_wait",
        json!({"targets":["agent-done","agent-failed","agent-running"]}),
    ));
    records.push(output(
        "call_wait",
        json!({"status":{"agent-done":{"completed":"PRIVATE_DONE"},"agent-failed":{"errored":"PRIVATE_ERROR"},"agent-running":"running"},"timed_out":false}),
    ));
    records.push(call(
        "close_agent",
        "call_close",
        json!({"target":"agent-closed"}),
    ));
    records.push(output("call_close", json!({"previous_status":"running"})));
    let [panel, _] = replay(
        ProviderKind::Codex,
        &records,
        ToolActivityDisplay::Summary,
        "",
    );
    let count = |row: &str| panel.matches(row).count();
    assert_eq!(
        [count("✓ Task"), count("✗ Task"), count("↗ Task")],
        [2, 1, 1],
        "{panel}"
    );
    assert!(
        panel.contains("실패 1") && !panel.contains("PRIVATE_"),
        "{panel}"
    );
}

#[test]
fn section_replay_codex_script_failure_marks_the_exec_row_failed() {
    let mut records = codex_records(1);
    records.push(
        json!({"timestamp":"2026-10-27T07:00:01.000Z","type":"response_item","payload":
        {"type":"custom_tool_call_output","call_id":"call_exec0","output":[
            {"type":"input_text","text":"Script failed\nWall time 0.1 seconds\nOutput:"},
            {"type":"input_text","text":"Error: 더미 실패"}]}}),
    );
    let [panel, _] = replay(
        ProviderKind::Codex,
        &records,
        ToolActivityDisplay::Summary,
        "",
    );
    assert!(panel.contains("실패 1") && panel.contains("✗ "), "{panel}");
}

// Replays one Claude tool call and returns the summary-mode card with mention breakers removed.
fn summary_card(name: &str, input: Value) -> String {
    let records = claude_tool("toolu_account", name, input);
    let [panel, _] = replay(
        ProviderKind::Claude,
        &records,
        ToolActivityDisplay::Summary,
        "",
    );
    panel.replace('\u{200b}', "")
}

#[test]
fn section_replay_cards_hide_account_operands_and_keep_package_versions() {
    for (name, input, shown) in [
        ("Bash", json!({"command":"su - LaneAlice"}), r"su - \*\*\*"),
        (
            "Bash",
            json!({"command":"chown LaneAlice:staff build.log"}),
            r"chown \*\*\* build.log",
        ),
        (
            "Bash",
            json!({"command":"sudo chown LaneAlice build.log"}),
            r"sudo chown \*\*\* build.log",
        ),
        (
            "Bash",
            json!({"command":"ssh LaneAlice@v2 uptime"}),
            r"ssh \*\*\*@v2 uptime",
        ),
        (
            "Bash",
            json!({"command":"ssh LaneAlice@canary uptime"}),
            r"ssh \*\*\*@canary uptime",
        ),
        (
            "Grep",
            json!({"pattern":"sudo -u LaneAlice make"}),
            r#""sudo -u \*\*\* make""#,
        ),
    ] {
        let card = summary_card(name, input.clone());
        assert!(
            card.contains(shown) && !card.contains("LaneAlice"),
            "{input}\n{card}"
        );
    }
    for (name, input, shown) in [
        (
            "Bash",
            json!({"command":"npm i react@18"}),
            "npm i react@18",
        ),
        (
            "Bash",
            json!({"command":"npx next@canary build"}),
            "npx next@canary build",
        ),
        (
            "Bash",
            json!({"command":"pnpm add react@experimental"}),
            "pnpm add react@experimental",
        ),
        (
            "Bash",
            json!({"command":"npm i node-lib@18.x"}),
            "npm i node-lib@18.x",
        ),
        (
            "Bash",
            json!({"command":"npm i some-lib@main"}),
            "npm i some-lib@main",
        ),
        (
            "Grep",
            json!({"pattern":"ssh -L 8080:localhost:80 build-host"}),
            r#""ssh -L 8080:localhost:80 build-host""#,
        ),
        (
            "Grep",
            json!({"pattern":"inspect chown handling"}),
            r#""inspect chown handling""#,
        ),
    ] {
        let card = summary_card(name, input.clone());
        assert!(card.contains(shown), "{input}\n{card}");
    }
}

#[test]
fn section_replay_cards_reset_account_context_per_command_and_line() {
    for (name, input, shown) in [
        (
            "Grep",
            json!({"pattern":"ssh -oUser=LaneAlice build-host"}),
            r#""ssh -oUser=\*\*\* build-host""#,
        ),
        (
            "Bash",
            json!({"command":"npm test && echo LaneAlice@canary"}),
            r"npm test && echo \*\*\*@canary",
        ),
        (
            "Bash",
            json!({"command":"npm test; echo LaneAlice@main"}),
            r"npm test; echo \*\*\*@main",
        ),
    ] {
        let card = summary_card(name, input.clone());
        assert!(
            card.contains(shown) && !card.contains("LaneAlice"),
            "{input}\n{card}"
        );
    }
    let todos = json!({"todos":[
        {"content":"chown LaneAlice:staff build.log","status":"pending","activeForm":"더미"},
        {"content":"su - LaneAlice","status":"pending","activeForm":"더미"},
        {"content":"fix su handling","status":"pending","activeForm":"더미"},
        {"content":"npm i react@18","status":"pending","activeForm":"더미"},
    ]});
    let card = summary_card("TodoWrite", todos);
    for shown in [
        "- [ ] chown *** build.log",
        "- [ ] su - ***",
        "- [ ] fix su handling",
        "- [ ] npm i react@18",
    ] {
        assert!(card.contains(shown), "{shown}\n{card}");
    }
    assert!(!card.contains("LaneAlice"), "{card}");
}
