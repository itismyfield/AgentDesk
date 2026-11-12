use super::*;
use crate::services::agent_protocol::StatusEvent;
use crate::services::discord::placeholder_live_events::{
    PlaceholderLiveEvents, status_events_from_json,
};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
use std::os::unix::fs::PermissionsExt;

fn child_path(r: &Rig, id: &str) -> PathBuf {
    r.path
        .with_extension("")
        .join("subagents")
        .join(format!("agent-{id}.jsonl"))
}
fn child_row(r: &Rig, id: &str, terminal: bool) {
    let path = child_path(r, id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut row = if terminal {
        json!({"type":"result","is_error":false})
    } else {
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}})
    };
    row["timestamp"] = json!(Utc::now());
    row["sessionId"] = json!(r.ctx.expected_native_session_id);
    row["agentId"] = json!(id);
    row["isSidechain"] = json!(true);
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .unwrap(),
        "{row}"
    )
    .unwrap();
}
fn launch(r: &Rig, live: &PlaceholderLiveEvents, id: &str) {
    let tool = format!("task-{id}");
    let row = json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":tool,
        "name":"Agent","input":{"description":id}}]}});
    r.append(row.clone());
    live.push_status_events(
        ChannelId::new(r.ctx.channel_id.unwrap()),
        status_events_from_json(&row),
    );
    let ack = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":tool,"content":"launched"}]},
        "toolUseResult":{"isAsync":true,"status":"async_launched","agentId":id,"totalTokens":0}});
    r.append(ack.clone());
    live.push_status_events(
        ChannelId::new(r.ctx.channel_id.unwrap()),
        status_events_from_json(&ack),
    );
    child_row(r, id, false);
}
fn fake_processes(tmux: &ScriptedTmux, error: bool) {
    let path = tmux.directory().join("ps");
    fs::write(
        &path,
        if error {
            "#!/bin/sh\nexit 1\n".to_string()
        } else {
            format!("#!/bin/sh\necho '{} 0'\n", std::process::id())
        },
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    let script = tmux.directory().join("tmux");
    let text = fs::read_to_string(&script).unwrap();
    if !text.contains("'#{pane_pid}')") {
        fs::write(
            script,
            text.replace(
                "case \"$last\" in",
                &format!(
                    "case \"$last\" in '#{{pane_pid}}') echo {} ;;",
                    std::process::id()
                ),
            ),
        )
        .unwrap();
    }
}
fn named() -> Rig {
    Rig::named(format!("AgentDesk-claude-b1-t{}", 8918081))
}
fn keep(r: &Rig) -> &'static str {
    crate::services::tmux_turn_liveness::kill_proven_idle_provider_session(
        &r.session,
        "fixture idle probe",
        || Err(None),
    )
    .expect_err("Present Claude must remain alive")
    .0
}
fn footer(live: &PlaceholderLiveEvents, r: &Rig) -> bool {
    live.render_completion_footer(
        ChannelId::new(r.ctx.channel_id.unwrap()),
        &ProviderKind::Claude,
        "x",
    )
    .has_unfinished_entries
}
async fn complete_parent(live: &PlaceholderLiveEvents, r: &Rig) -> bool {
    r.append(json!({"type":"system","subtype":"turn_duration"}));
    assert_eq!(collector::observe(&r.session).foreground, Foreground::Idle);
    let pending = crate::services::discord::tmux::sniff_background_agent_pending_for_completion(
        Some(&r.session),
    )
    .await;
    live.push_status_event(
        ChannelId::new(r.ctx.channel_id.unwrap()),
        StatusEvent::TurnCompleted {
            background: false,
            background_agent_pending: pending,
        },
    );
    pending
}
#[tokio::test]
async fn async_ack_is_not_footer_completion_and_child_terminal_repairs_lost_stop() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let r = named();
    let tmux = ScriptedTmux::new("");
    fake_processes(&tmux, false);
    let live = PlaceholderLiveEvents::default();
    launch(&r, &live, "child-a");
    let pending = complete_parent(&live, &r).await;
    assert!(
        footer(&live, &r),
        "async ack must never mark the live child footer complete"
    );
    assert!(
        pending,
        "current transcript child must survive an empty/narrow/capture-failed screen"
    );
    let original = fs::read(&r.path).unwrap();
    assert_eq!(keep(&r), "ChildPending");
    for screen in ["", "  1 background task", "❯\ncontext\nfooter"] {
        tmux.show(screen);
        assert!(
            crate::services::discord::tmux::sniff_background_agent_pending_for_completion(Some(
                &r.session
            ))
            .await
        );
    }
    child_row(&r, "child-a", true);
    collector::poll_for_tests();
    assert_eq!(fs::read(&r.path).unwrap(), original);
    assert!(
        !footer(&live, &r),
        "exact child terminal must finish footer even when Stop was lost"
    );
    assert_eq!(keep(&r), "UnprovenInteractiveState");
    let calls = fs::read_to_string(tmux.directory().join("argv")).unwrap_or_default();
    assert!(!calls.contains("capture-pane") && !calls.contains("kill-session"));
}
#[tokio::test]
async fn child_a_stop_cannot_complete_b_or_parent_and_last_stop_keeps_interactive_state() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let r = named();
    let tmux = ScriptedTmux::new("");
    fake_processes(&tmux, false);
    let live = PlaceholderLiveEvents::default();
    launch(&r, &live, "child-a");
    launch(&r, &live, "child-b");
    complete_parent(&live, &r).await;
    for (id, pending) in [("child-a", true), ("child-b", false)] {
        r.hook_payload("SubagentStop",json!({"session_id":r.ctx.expected_native_session_id,"transcript_path":r.path,
            "hook_event_name":"SubagentStop","agent_id":id,"agent_transcript_path":child_path(&r,id)})).await;
        collector::poll_for_tests();
        assert_eq!(
            footer(&live, &r),
            pending,
            "child A Stop must not finish sibling B's footer"
        );
        assert_eq!(
            collector::observe(&r.session).foreground,
            Foreground::Idle,
            "child Stop cannot mutate parent"
        );
        assert_eq!(
            keep(&r),
            if pending {
                "ChildPending"
            } else {
                "UnprovenInteractiveState"
            }
        );
    }
    let calls = fs::read_to_string(tmux.directory().join("argv")).unwrap_or_default();
    assert!(!calls.contains("capture-pane") && !calls.contains("kill-session"));
}
#[tokio::test]
async fn present_claude_never_captures_or_kills_for_idle_busy_unknown_or_ps_failure() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let tmux = ScriptedTmux::new("Ready for input (type message + Enter)\n> ");
    for state in 0..5 {
        let r = named();
        fake_processes(&tmux, state == 3);
        let expected = if state == 1 {
            r.hook("PreToolUse").await;
            Foreground::Tool
        } else if state == 2 {
            Foreground::Unknown
        } else {
            r.append(json!({"type":"system","subtype":"turn_duration"}));
            Foreground::Idle
        };
        assert_eq!(collector::observe(&r.session).foreground, expected);
        assert_eq!(
            crate::services::platform::tmux::session_presence(&r.session),
            crate::services::platform::tmux::SessionPresence::Present
        );
        assert_eq!(keep(&r), "UnprovenInteractiveState");
    }
    let calls = fs::read_to_string(tmux.directory().join("argv")).unwrap_or_default();
    assert!(
        !calls.contains("capture-pane"),
        "Present Claude never enters the screen-based idle proof"
    );
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("kill-session"))
            .count(),
        0
    );
}

#[tokio::test]
async fn restart_with_unassigned_async_ack_never_publishes_an_empty_child_set() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let r = named();
    let tmux = ScriptedTmux::new("");
    fake_processes(&tmux, false);
    r.append(json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"missing-launch","content":"launched"}]},
        "toolUseResult":{"isAsync":true,"agentId":"child-a"}}));
    let live = PlaceholderLiveEvents::default();
    let pending = complete_parent(&live, &r).await;
    assert!(
        footer(&live, &r),
        "an unassigned launch cannot become an empty completed footer"
    );
    assert!(pending);
    assert_eq!(keep(&r), "UnprovenInteractiveState");
}
