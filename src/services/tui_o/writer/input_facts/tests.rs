use std::collections::BTreeSet;
use std::io::Write;
use std::sync::Mutex;

use serde_json::{Value, json};

use super::reactions::{InputReaction, ReactionPort, reconcile};
use super::*;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::file_identity;

fn binding(path: &std::path::Path, provider: ShadowProvider, channel_id: u64) -> SourceBinding {
    let (dev, ino) = file_identity(&std::fs::metadata(path).unwrap());
    SourceBinding {
        channel_id,
        provider,
        source: SourceId {
            session_id: "parent".into(),
            path: path.into(),
            dev,
            ino,
        },
    }
}

fn append(path: &std::path::Path, records: &[Value]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for record in records {
        writeln!(file, "{record}").unwrap();
    }
}

fn user(id: &str, text: &str) -> Value {
    json!({"type":"user", "uuid":id, "message":{"content":text}})
}

fn idle() -> Value {
    json!({"type":"system", "subtype":"turn_duration"})
}

fn codex(kind: &str, id: &str) -> Value {
    json!({"type":"event_msg", "payload":{"type":kind, "turn_id":id}})
}

#[test]
fn delayed_supply_replays_order_and_never_exposes_backlog_idle() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    std::fs::write(&path, "").unwrap();
    let binding = binding(&path, ShadowProvider::Claude, 21);
    let mut facts = InputFacts::open(binding.clone()).unwrap();
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Unknown);
    append(&path, &[user("a", "first"), idle(), user("b", "second")]);
    let first_end = format!("{}\n", user("a", "first")).len() as u64;
    let close_end = format!("{}\n", idle()).len() as u64;
    assert_eq!(
        facts.poll(first_end).unwrap().state,
        TurnState::Open {
            native_turn_id: Some("a".into())
        }
    );
    assert_eq!(facts.poll(close_end).unwrap().state, TurnState::Unknown);
    let open = facts.poll(u64::MAX).unwrap();
    assert_eq!(
        open.state,
        TurnState::Open {
            native_turn_id: Some("b".into())
        }
    );
    assert_eq!(
        InputFacts::open(binding.clone())
            .unwrap()
            .poll(u64::MAX)
            .unwrap(),
        open
    );
    append(&path, &[idle()]);
    let closed = facts.poll(u64::MAX).unwrap();
    assert_eq!(closed.state, TurnState::Idle);
    assert_eq!(
        InputFacts::open(binding).unwrap().poll(u64::MAX).unwrap(),
        closed
    );
}

#[test]
fn claude_parent_tools_and_background_notification_ignore_child_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    let children = root.path().join("subagents");
    std::fs::create_dir(&children).unwrap();
    let child = children.join("agent.jsonl");
    std::fs::write(&child, "unreadable child contents\n").unwrap();
    std::fs::write(&path, "").unwrap();
    let binding = binding(&path, ShadowProvider::Claude, 22);
    let mut facts = InputFacts::open(binding).unwrap();
    let tool = json!({"type":"assistant","uuid":"tool","apiBlockIndex":0,
        "message":{"id":"m","content":[{"type":"tool_use","id":"t","name":"Agent","input":{}}]}});
    let result = json!({"type":"user","uuid":"result","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"done"}]}});
    append(&path, &[user("parent", "run agent"), tool, result]);
    assert_eq!(
        facts.poll(u64::MAX).unwrap().state,
        TurnState::Open {
            native_turn_id: Some("parent".into())
        }
    );
    assert!(InputFacts::open(super::tests::binding(&child, ShadowProvider::Claude, 22)).is_err());
    append(&path, &[idle()]);
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Idle);
    append(
        &path,
        &[user(
            "notice",
            "<task-notification>child finished</task-notification>",
        )],
    );
    assert_eq!(
        facts.poll(u64::MAX).unwrap().state,
        TurnState::Open {
            native_turn_id: Some("notice".into())
        }
    );
    append(&path, &[idle()]);
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Idle);
}

#[test]
fn codex_parent_stays_open_during_child_completion_and_foreign_closer() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    let child = root.path().join("child.jsonl");
    let header: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/tui_input/codex-subagent-session-meta.json"
    ))
    .unwrap();
    std::fs::write(&child, format!("{header}\n")).unwrap();
    append(
        &child,
        &[
            codex("task_started", "child"),
            codex("task_complete", "child"),
        ],
    );
    std::fs::write(&path, "").unwrap();
    let mut facts = InputFacts::open(binding(&path, ShadowProvider::Codex, 23)).unwrap();
    assert!(InputFacts::open(binding(&child, ShadowProvider::Codex, 23)).is_err());
    append(
        &path,
        &[
            codex("task_started", "parent"),
            json!({"type":"response_item","payload":{"type":"function_call","id":"spawn","name":"spawn_agent","arguments":"{}"}}),
            json!({"type":"response_item","payload":{"type":"function_call","id":"wait","name":"wait_agent","arguments":"{}"}}),
        ],
    );
    let open = TurnState::Open {
        native_turn_id: Some("parent".into()),
    };
    assert_eq!(facts.poll(u64::MAX).unwrap().state, open);
    append(&path, &[codex("task_complete", "foreign")]);
    assert_eq!(facts.poll(u64::MAX).unwrap().state, open);
    append(&path, &[codex("task_complete", "parent")]);
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Idle);
}

#[test]
fn channels_are_isolated_and_partial_or_corrupt_supply_is_not_idle() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a.jsonl");
    let b = root.path().join("b.jsonl");
    std::fs::write(&a, "").unwrap();
    std::fs::write(&b, "").unwrap();
    append(&a, &[user("a", "busy")]);
    append(&b, &[idle()]);
    let mut a = InputFacts::open(binding(&a, ShadowProvider::Claude, 24)).unwrap();
    let mut facts = InputFacts::open(binding(&b, ShadowProvider::Claude, 25)).unwrap();
    assert!(matches!(
        a.poll(u64::MAX).unwrap().state,
        TurnState::Open { .. }
    ));
    assert_eq!(facts.poll(u64::MAX).unwrap().binding.channel_id, 25);
    let mut file = std::fs::OpenOptions::new().append(true).open(&b).unwrap();
    write!(file, "{{\"type\":\"user\"").unwrap();
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Unknown);
    writeln!(file, "broken").unwrap();
    assert!(facts.poll(u64::MAX).is_err());
    assert!(
        facts.poll(u64::MAX).is_err(),
        "corruption must remain halted"
    );
}

#[derive(Default)]
struct Reactions {
    visible: Mutex<BTreeSet<(u64, u64, char)>>,
    calls: Mutex<Vec<(char, bool)>>,
    fail: Mutex<Option<(char, bool)>>,
}

impl ReactionPort for Reactions {
    async fn set(
        &self,
        channel: u64,
        message: u64,
        emoji: char,
        present: bool,
    ) -> Result<(), String> {
        self.calls.lock().unwrap().push((emoji, present));
        if *self.fail.lock().unwrap() == Some((emoji, present)) {
            return Err("reaction unavailable".into());
        }
        let mut visible = self.visible.lock().unwrap();
        if present {
            visible.insert((channel, message, emoji));
        } else {
            visible.remove(&(channel, message, emoji));
        }
        Ok(())
    }
}

#[tokio::test]
async fn reactions_converge_on_replay_and_propagate_partial_failure() {
    let port = Reactions::default();
    for (state, emoji) in [
        (InputReaction::Pending, '\u{23f3}'),
        (InputReaction::Done, '\u{2705}'),
        (InputReaction::Warning, '\u{26a0}'),
    ] {
        reconcile(&port, 25, 26, state).await.unwrap();
        reconcile(&port, 25, 26, state).await.unwrap();
        assert_eq!(
            *port.visible.lock().unwrap(),
            BTreeSet::from([(25, 26, emoji)])
        );
    }
    let warning = BTreeSet::from([(25, 26, '\u{26a0}')]);
    assert_eq!(*port.visible.lock().unwrap(), warning);
    *port.fail.lock().unwrap() = Some(('\u{2705}', true));
    assert!(reconcile(&port, 25, 26, InputReaction::Done).await.is_err());
    assert_eq!(*port.visible.lock().unwrap(), warning);
    *port.fail.lock().unwrap() = Some(('\u{26a0}', false));
    assert!(reconcile(&port, 25, 26, InputReaction::Done).await.is_err());
    *port.fail.lock().unwrap() = None;
    reconcile(&port, 25, 26, InputReaction::Done).await.unwrap();
    assert_eq!(
        *port.visible.lock().unwrap(),
        BTreeSet::from([(25, 26, '\u{2705}')])
    );
    port.calls.lock().unwrap().clear();
    assert!(reconcile(&port, 25, 0, InputReaction::Done).await.is_err());
    assert!(port.calls.lock().unwrap().is_empty());
}

#[test]
fn g0_discovery_install_and_o_binding_supply_only_the_new_parent() {
    use crate::config::TestEnvVarGuard;
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::codex_tui::{rollout_tail, session};
    use crate::services::tui_o::writer::binding::{
        BindingEvents, BindingRecord, BindingTarget, ChannelBindingLog,
    };
    use crate::services::tui_prompt_dedupe as dedupe;
    use std::time::{Duration, SystemTime};

    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _home = TestEnvVarGuard::set_path_after_shared_test_env_lock("CODEX_HOME", root.path());
    let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            dedupe::binding_events::set_test_root(None);
            dedupe::reset_state_for_tests();
        }
    }
    let _restore = Restore;
    let sessions = root.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let write = |id: &str, header: Value, time: u64| {
        let path = sessions.join(format!("rollout-{id}.jsonl"));
        std::fs::write(&path, format!("{header}\n")).unwrap();
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(time))
            .unwrap();
        path
    };
    let parent = |id: &str| json!({"type":"session_meta","payload":{"id":id,"cwd":root.path(),"source":"cli","originator":"codex-tui"}});
    write("a", parent("a"), 10);
    let b = write("b", parent("b"), 20);
    let mut child: Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/tui_input/codex-subagent-session-meta.json"
    ))
    .unwrap();
    child["payload"]["cwd"] = json!(root.path());
    write("child", child, 30);
    let selected = rollout_tail::latest_rollout_for_cwd_since(
        root.path(),
        SystemTime::UNIX_EPOCH + Duration::from_secs(15),
        &sessions,
    )
    .unwrap();
    dedupe::register_tmux_channel("input-facts-parent-test", 27);
    session::install_launched_codex_tui_runtime_binding(
        "input-facts-parent-test",
        Some(0),
        dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: selected.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("b".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let events = ChannelBindingLog::new(27, ShadowProvider::Codex)
        .binding_events_since(27, 0)
        .unwrap();
    let [event] = events.as_slice() else {
        panic!("one parent binding required")
    };
    let BindingRecord::Bound {
        new: BindingTarget::Source(source),
        ..
    } = &event.record
    else {
        panic!("resolved parent required")
    };
    assert_eq!(source.path, b);
    let mut facts = InputFacts::open(SourceBinding {
        channel_id: 27,
        provider: ShadowProvider::Codex,
        source: source.clone(),
    })
    .unwrap();
    append(&b, &[codex("task_started", "b")]);
    assert_eq!(
        facts.poll(u64::MAX).unwrap().state,
        TurnState::Open {
            native_turn_id: Some("b".into())
        }
    );
    append(&b, &[codex("task_complete", "b")]);
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Idle);
}

#[test]
fn anonymous_openers_and_sidechain_contamination_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    std::fs::write(&path, "").unwrap();
    let mut facts = InputFacts::open(binding(&path, ShadowProvider::Claude, 28)).unwrap();
    append(
        &path,
        &[
            idle(),
            json!({"type":"user","message":{"content":"anonymous"}}),
        ],
    );
    assert_eq!(
        facts.poll(u64::MAX).unwrap().state,
        TurnState::Open {
            native_turn_id: None
        }
    );
    append(&path, &[idle()]);
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Idle);
    let mut child = idle();
    child["isSidechain"] = json!(true);
    append(&path, &[child]);
    assert!(facts.poll(u64::MAX).is_err());
}
