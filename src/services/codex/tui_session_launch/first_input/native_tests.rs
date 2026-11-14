use super::*;
use crate::services::tui_o::channel_policy::first_prompt_digest;
use crate::test_support::wait::{WAIT_CAP, join_within, recv_within};
use std::io::Write;

#[test]
fn fresh_observe_publishes_the_pinned_source_before_tail_and_finishes_a_short_turn() {
    native_scenario(true);
}

#[test]
fn unprovable_early_discovery_releases_its_admission_and_preserves_the_legacy_tail() {
    native_scenario(false);
}

fn native_scenario(provable: bool) {
    let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let state_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    dedupe::reset_state_for_tests();
    let (root, guards) = fixture::fixture_after_shared_test_env_lock();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let tmux = fixture::fake_tmux(root.path());
    std::fs::write(root.path().join("tmux"), r#"#!/bin/bash
if [ "${1-}" = -u ]; then shift; fi
printf '%s\n' "$*" >> "$AGENTDESK_ROOT_DIR/tmux.calls"
case "${1-}" in
list-panes) printf '0\n' ;;
capture-pane) printf 'some earlier output\nmore output\n╭──────────────────────────────────────────────────────────────╮\n│ ▌                                                            │\n╰──────────────────────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send\n' ;;
display-message)
    case "${@: -1}" in
    '#{pane_dead}') printf '0\n' ;;
    '#{pane_width} #{pane_height}') printf '80 24\n' ;;
    esac ;;
esac
"#).unwrap();
    crate::services::platform::tmux::session_identity_tests::mark_fixture_program(
        root.path().join("tmux"),
    );
    let sessions = root.path().join("pinned/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let wrong_home = root.path().join("other");
    std::fs::create_dir_all(wrong_home.join("sessions")).unwrap();
    let home = Guard::set_path_after_shared_test_env_lock("CODEX_HOME", &wrong_home);
    let hooks = Guard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
        "0".as_ref(),
    );
    let name = "codex-early-discovery-fixture";
    let channel = 8_165_501;
    let id = "00000000-0000-4000-8000-000000008165";
    let prepared = PreparedIncarnation::prepare_pinned(
        "codex",
        name,
        Some(channel),
        None,
        false,
        Some(sessions.canonicalize().unwrap()),
        (Some(first_prompt_digest("hi")), Some("legacy".into())),
    )
    .unwrap();
    let context = prepared.context.clone();
    let nonce = crate::services::tmux_common::session_temp_path(name, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&nonce).parent().unwrap()).unwrap();
    std::fs::write(nonce, &context.execution_nonce).unwrap();
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        name,
        crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
    )
    .unwrap();
    dedupe::register_tmux_channel(name, channel);
    let path = sessions.join(format!("rollout-{id}.jsonl"));
    let records = [
        serde_json::json!({"type":"session_meta", "timestamp":chrono::Utc::now().to_rfc3339(), "payload":{"id":id,"cwd":root.path(),"source":"cli","timestamp":chrono::Utc::now().to_rfc3339()}}),
        serde_json::json!({"type":"event_msg", "payload":{"type":"task_started","turn_id":"t1"}}),
        serde_json::json!({"type":"response_item", "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}),
    ];
    let mut records = records;
    if !provable {
        records[0]["payload"]["timestamp"] = "1970-01-01T00:00:00Z".into();
    }
    let text = records.iter().map(|v| format!("{v}\n")).collect::<String>();
    std::fs::write(&path, &text).unwrap();
    // A newer same-cwd file in the mutable default home must never replace the launch's source.
    let other_id = "00000000-0000-4000-8000-000000008166";
    std::fs::write(
        wrong_home
            .join("sessions")
            .join(format!("rollout-{other_id}.jsonl")),
        text.replace(id, other_id),
    )
    .unwrap();
    let selection = crate::services::codex_tui::session::resolve_codex_tui_session(
        None,
        root.path(),
        None,
        true,
    );
    let tail =
        crate::services::codex_tui::tail_launch::validate_selection(&context, &selection).unwrap();
    let handoff = dedupe::reserve_discord_handoff("codex", name, "hi");
    let (sender, receiver) = std::sync::mpsc::channel();
    let cwd = root.path().display().to_string();
    let started = std::time::Instant::now();
    let worker_context = context.clone();
    let event_root = root.path().to_path_buf();
    let worker = std::thread::spawn(move || {
        use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
        dedupe::binding_events::set_test_root(Some(&event_root));
        let config = serde_json::from_value(serde_json::json!({"server":{},
            "agents":[{"id":"early", "name":"early", "channels":{"codex":{"id":channel.to_string(),"runtime":"tui"}}}],
            "tui_o":{"writer":{"channels":[channel],"late_adopt":true}}})).unwrap();
        let boot = BootChannels::validate(&config)
            .unwrap()
            .seeded(&config, |_, _| {
                Ok(std::collections::BTreeMap::from([(
                    channel,
                    Adoption::Pending,
                )]))
            })
            .unwrap();
        let candidate = boot.output_candidate(channel).unwrap();
        let boot_guard = crate::services::tui_o::cutover::test_override::force_boot(boot);
        let token = std::sync::Arc::new(CancelToken::new());
        let nonce = token.turn_nonce().unwrap().to_owned();
        let mut admission = candidate
            .admit_first(channel, token.turn_nonce().unwrap(), "hi", name, || Ok(0))
            .unwrap();
        candidate.attach_first_execution(&worker_context, "hi", token.turn_nonce());
        let result = observe(Observe {
            submitted: Submitted {
                prepared,
                validated_tail: tail,
                rollout_modified_since: std::time::UNIX_EPOCH,
                handoff,
            },
            working_dir: &cwd,
            sender,
            cancel_token: Some(token),
            tmux_session_name: name,
            prompt: "hi",
        });
        if result.is_ok() {
            admission.completed();
        }
        drop(admission);
        let snapshot = crate::services::tui_o::cutover::boot_snapshot().unwrap();
        drop(boot_guard);
        dedupe::binding_events::set_test_root(None);
        (result, candidate, nonce, snapshot)
    });
    let init = recv_within(&receiver, WAIT_CAP);
    let published = dedupe::runtime_binding_for_tmux_session(name);
    let pending_before_output = !worker.is_finished();
    let before = dedupe::binding_events::records_strict(channel).unwrap();
    for target in [
        &path,
        &wrong_home
            .join("sessions")
            .join(format!("rollout-{other_id}.jsonl")),
    ] {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(target)
            .unwrap();
        for value in [
            serde_json::json!({"type":"response_item", "payload":{"type":"message","role":"assistant","id":"answer","content":[{"type":"output_text","text":"quick answer"}]}}),
            serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete","turn_id":"t1"}}),
        ] {
            writeln!(file, "{value}").unwrap();
        }
    }
    let (result, candidate, nonce, snapshot) = join_within(worker, WAIT_CAP);
    result.unwrap();
    assert_eq!(
        candidate.first_mark().is_some(),
        provable,
        "a failed early proof must not be revived by post-tail publication"
    );
    let scope = crate::services::tui_o::cutover::test_override::force_boot(snapshot);
    candidate.invalidate_first(Some(&nonce), None);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sent = runtime
        .block_on(crate::test_support::wait::await_within(
            crate::services::tui_o::cutover::claim_then_send(
                Some(
                    crate::services::tui_o::cutover::BodyClaim::new(
                        channel,
                        Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui),
                    )
                    .for_turn(Some(&nonce)),
                ),
                || async { 1 },
            ),
            WAIT_CAP,
        ))
        .unwrap();
    assert!(matches!(
        sent,
        crate::services::tui_o::cutover::BodySend::Sent(1)
    ));
    assert_eq!(candidate.sends(), (1, 1));
    drop(scope);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "short native turn must finish under two seconds"
    );
    assert!(
        pending_before_output,
        "discovery publishes while the tail still waits for output"
    );
    if provable {
        let published = published.expect("fresh discovery must publish before its first output");
        assert_eq!(
            published.output_path,
            path.canonicalize().unwrap().display().to_string()
        );
        assert_eq!(published.session_id.as_deref(), Some(id));
    } else {
        assert!(published.is_none());
        assert!(before.as_ref().unwrap().is_empty());
    }
    assert!(matches!(init, StreamMessage::Init { .. }));
    let messages: Vec<_> = receiver.into_iter().collect();
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(
                m,
                StreamMessage::Done { .. } | StreamMessage::CodexTuiTerminalDone { .. }
            ))
            .count(),
        1
    );
    assert_eq!(messages.iter().filter(|message| matches!(message, StreamMessage::Text { content } if content.contains("quick answer"))).count(), 1);
    if !provable {
        let source =
            crate::services::tui_o::shadow::binding_reader::source_id_for(id, &path).unwrap();
        assert!(candidate.first_proof(&source).is_err());
        dedupe::reset_state_for_tests();
        dedupe::binding_events::set_test_root(None);
        return;
    }
    assert_eq!(
        dedupe::binding_events::records_strict(channel).unwrap(),
        before,
        "post-tail install is the same source, not a rebind"
    );
    crate::services::codex_tui::session::publish_discovered_launch(&context, &path).unwrap();
    let original = std::fs::read(&path).unwrap();
    for timestamp in [
        serde_json::Value::Null,
        serde_json::json!("1970-01-01T00:00:00Z"),
    ] {
        let mut lines: Vec<serde_json::Value> = std::str::from_utf8(&original)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        lines[0]["payload"]["timestamp"] = timestamp;
        std::fs::write(
            &path,
            lines.iter().map(|v| format!("{v}\n")).collect::<String>(),
        )
        .unwrap();
        assert!(
            crate::services::codex_tui::session::publish_discovered_launch(&context, &path)
                .is_err()
        );
        std::fs::write(&path, &original).unwrap();
    }
    let resumed = PreparedIncarnation::prepare_pinned(
        "codex",
        name,
        Some(channel),
        Some(id),
        true,
        context.provider_root.clone(),
        (Some(first_prompt_digest("hi")), Some("legacy".into())),
    )
    .unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(name, "spawn_nonce"),
        &resumed.context.execution_nonce,
    )
    .unwrap();
    assert!(
        crate::services::codex_tui::session::publish_discovered_launch(&resumed.context, &path)
            .is_err()
    );
    std::fs::write(
        crate::services::tmux_common::session_temp_path(name, "spawn_nonce"),
        &context.execution_nonce,
    )
    .unwrap();
    let events = dedupe::binding_events::records_strict(channel).unwrap();
    for marker in [None, Some("successor")] {
        let marker_path = crate::services::tmux_common::session_temp_path(name, "spawn_nonce");
        match marker {
            None => std::fs::remove_file(&marker_path).unwrap(),
            Some(nonce) => std::fs::write(&marker_path, nonce).unwrap(),
        }
        assert!(
            crate::services::codex_tui::session::publish_discovered_launch(&context, &path)
                .is_err()
        );
        std::fs::write(marker_path, &context.execution_nonce).unwrap();
    }
    assert!(
        crate::services::codex_tui::session::publish_discovered_launch(
            &context,
            &wrong_home
                .join("sessions")
                .join(format!("rollout-{other_id}.jsonl"))
        )
        .is_err()
    );
    let log = dedupe::binding_events::log_path(channel).unwrap().unwrap();
    std::fs::rename(&log, log.with_extension("saved")).unwrap();
    std::fs::create_dir(&log).unwrap();
    dedupe::binding_events::forget_channel_for_tests(channel);
    assert!(
        crate::services::codex_tui::session::publish_discovered_launch(&context, &path).is_err(),
        "early publication must not inherit the legacy install's success on an append failure"
    );
    std::fs::remove_dir(&log).unwrap();
    std::fs::rename(log.with_extension("saved"), &log).unwrap();
    dedupe::binding_events::forget_channel_for_tests(channel);
    assert_eq!(
        dedupe::binding_events::records_strict(channel).unwrap(),
        events
    );
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(None);
    drop(hooks);
    drop(home);
    drop(tmux);
    drop(guards);
    drop(state_lock);
    drop(env_lock);
}
