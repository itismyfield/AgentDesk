//! #5808 C2: allocation identity only; no full-watcher or delivery-arrival claim.
use super::super::pre_emit_guard::*;
use super::*;
use crate::services::discord::{TmuxWatcherHandle, make_shared_data_for_tests};
use std::sync::{
    Mutex,
    atomic::{AtomicI64, AtomicU64},
};

fn handle(session: &str, output: &str) -> TmuxWatcherHandle {
    TmuxWatcherHandle {
        tmux_session_name: session.into(),
        output_path: output.into(),
        paused: Arc::new(AtomicBool::new(false)),
        resume_offset: Arc::new(Mutex::new(Some(73))),
        cancel: Arc::new(AtomicBool::new(false)),
        pause_epoch: Arc::new(AtomicU64::new(19)),
        turn_delivered: Arc::new(AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(AtomicI64::new(0)),
    }
}

async fn stop(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    a: &TmuxWatcherHandle,
    committed: bool,
    paused: bool,
) -> TerminalCommitEpilogueOutcome {
    let context = TerminalCommitEpilogueContext {
        shared,
        channel_id,
        watcher_provider: &ProviderKind::Claude,
        provider_kind: &ProviderKind::Claude,
        tmux_session_name: &a.tmux_session_name,
        output_path: &a.output_path,
        relay_coord: &Arc::new(TmuxRelayCoord::new(channel_id)),
        turn_delivered: &a.turn_delivered,
    };
    run_terminal_commit_epilogue(
        &context,
        TerminalCommitEpilogueLocals {
            terminal_output_committed: committed,
            lifecycle_stage_paused: paused,
            relay_suppressed: false,
            has_assistant_response: false,
            completion_is_stale_for_newer_turn: true,
            anchor_cleanup_is_stale_for_newer_turn: true,
            inflight_state: &None,
            inflight_before_relay: &None,
            full_response: &String::new(),
            watcher_turn_nonce: &None,
            resolved_did: &None,
            dispatch_ok: true,
            terminal_delivery_committed: true,
            watcher_tui_gate_outcome: TuiCompletionGateOutcome::NotGated,
            tui_direct_anchor_terminal_body_visible: false,
            terminal_kind: None,
            terminal_evidence_offset: None,
            finish_mailbox_on_completion: true,
            pre_panel_release_drove_finalize: false,
            current_offset: 0,
            data_start_offset: 0,
        },
        &mut TerminalCommitEpilogueState {
            turn_result_relayed: &mut false,
            watcher_direct_terminal_idle_committed: &mut true,
            monitor_auto_turn_claimed: &mut false,
            monitor_auto_turn_finished: &mut false,
            monitor_auto_turn_synthetic_msg_id: &mut None,
            monitor_auto_turn_ledger_generation: &mut None,
        },
    )
    .await
}

async fn guard(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    h: &TmuxWatcherHandle,
    buffer: &mut String,
) -> PreEmitGuardOutcome {
    run_pre_emit_guard(
        &PreEmitGuardContext {
            http: &Arc::new(serenity::Http::new("fixture-no-network")),
            shared,
            channel_id,
            watcher_provider: &ProviderKind::Claude,
            tmux_session_name: &h.tmux_session_name,
            output_path: &h.output_path,
            paused: &h.paused,
            pause_epoch: &h.pause_epoch,
            turn_delivered: &h.turn_delivered,
        },
        PreEmitGuardLocals {
            epoch_snapshot: 19,
            monitor_auto_turn_deferred: false,
            placeholder_msg_id: None,
            turn_data_start_offset: 0,
            current_offset: 0,
            response_sent_offset: 0,
            data_start_offset: 0,
            stale_resume_detected: false,
            last_edit_text: &String::new(),
        },
        &mut PreEmitGuardState {
            monitor_auto_turn_claimed: &mut false,
            monitor_auto_turn_finished: &mut false,
            monitor_auto_turn_synthetic_msg_id: &mut None,
            monitor_auto_turn_ledger_generation: &mut None,
            all_data: buffer,
            all_data_start_offset: &mut 0,
            all_data_fully_mirrored_to_session_relay: &mut false,
            all_data_session_bound_relay_ack: &mut None,
            all_data_first_forwarded_relay_sequence: &mut None,
            last_relayed_offset: &mut None,
            last_observed_generation_mtime_ns: &mut None,
            full_response: &mut String::new(),
        },
    )
    .await
}

// PATH is replaced only in the child, never in the parallel test runner.
#[cfg(unix)]
fn isolated(name: &str) {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let tmux = root.path().join("tmux");
    std::fs::write(
        &tmux,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$AGENTDESK_ROOT_DIR/probes\"\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    let exact = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture"])
        .env("ADK_C2_FIXTURE_CHILD", name)
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
}

#[cfg(unix)]
async fn exercise(name: &str, same: bool, committed: bool, paused: bool) {
    if std::env::var("ADK_C2_FIXTURE_CHILD").as_deref() != Ok(name) {
        isolated(name);
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    assert!(root.join("tmux").is_file());
    let channel = serenity::ChannelId::new(58_080_000 + u64::from(std::process::id()));
    let session = format!("c2-fixture-{}", channel.get());
    let output = root.join("output.jsonl");
    std::fs::write(&output, "").unwrap();
    let a = handle(&session, output.to_str().unwrap());
    let b = handle(&session, output.to_str().unwrap());
    assert!(!Arc::ptr_eq(&a.turn_delivered, &b.turn_delivered));
    assert!(!Arc::ptr_eq(&a.cancel, &b.cancel));
    let shared = make_shared_data_for_tests();
    let current = if same { &a } else { &b };
    shared.tmux_watchers.insert(
        channel,
        TmuxWatcherHandle {
            tmux_session_name: current.tmux_session_name.clone(),
            output_path: current.output_path.clone(),
            paused: current.paused.clone(),
            resume_offset: current.resume_offset.clone(),
            cancel: current.cancel.clone(),
            pause_epoch: current.pause_epoch.clone(),
            turn_delivered: current.turn_delivered.clone(),
            last_heartbeat_ts_ms: current.last_heartbeat_ts_ms.clone(),
        },
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        stop(&shared, channel, &a, committed, paused),
    )
    .await
    .unwrap();
    let stopped = committed && !paused;
    assert_eq!(
        result,
        if stopped {
            TerminalCommitEpilogueOutcome::BreakWatcherLoop
        } else {
            TerminalCommitEpilogueOutcome::Fallthrough
        }
    );
    assert_eq!(*b.resume_offset.lock().unwrap(), Some(73));
    assert!(!b.paused.load(Ordering::Acquire));
    assert!(!b.cancel.load(Ordering::Acquire));
    assert_eq!(b.pause_epoch.load(Ordering::Acquire), 19);
    let mut buffer = "새 세대의 한국어 응답을 보존합니다.".to_string();
    let h = if same { &a } else { &b };
    let decision = guard(&shared, channel, h, &mut buffer).await;
    if same && stopped {
        assert_eq!(decision, PreEmitGuardOutcome::ContinueWatcherLoop);
        assert!(
            buffer.is_empty(),
            "same generation suppresses duplicate buffer"
        );
    } else {
        assert_eq!(
            (
                b.turn_delivered.load(Ordering::Acquire),
                decision,
                buffer.as_str()
            ),
            (
                false,
                PreEmitGuardOutcome::Proceed,
                "새 세대의 한국어 응답을 보존합니다."
            ),
            "B marker and actual guard must preserve successor prose"
        );
    }
    assert_eq!(
        a.turn_delivered.load(Ordering::Acquire),
        stopped,
        "A actual Stop writer"
    );
    assert!(
        !b.turn_delivered.load(Ordering::Acquire),
        "late A must not stamp B"
    );
    assert_eq!(
        root.join("probes").exists(),
        stopped,
        "actual liveness probe"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn late_a_preserves_successor_b() {
    exercise("late_a_preserves_successor_b", false, true, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn same_a_suppresses_duplicate() {
    exercise("same_a_suppresses_duplicate", true, true, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn pending_preserves_markers() {
    exercise("pending_preserves_markers", false, false, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn paused_preserves_markers() {
    exercise("paused_preserves_markers", false, true, true).await;
}

#[test]
fn caller_arc_wiring() {
    // Lexical supplement: does not execute the whole tmux_output_watcher task.
    let source = include_str!("../../tmux_watcher.rs");
    let context = source
        .split("let terminal_commit_epilogue_context = TerminalCommitEpilogueContext {")
        .nth(1)
        .unwrap()
        .split("};")
        .next()
        .unwrap();
    assert!(context.contains("turn_delivered: &turn_delivered,"));
    assert!(!context.contains("tmux_watchers"));
    let writer = include_str!("../terminal_commit_epilogue.rs");
    assert!(writer.contains("let turn_delivered = context.turn_delivered;"));
    assert!(!writer.contains("shared.tmux_watchers"));
}

#[cfg(unix)]
async fn exercise_zero_id_cleanup(name: &str, newer: bool) {
    use crate::services::discord::{inflight, tui_direct_abort_marker};
    if std::env::var("ADK_C2_FIXTURE_CHILD").as_deref() != Ok(name) {
        isolated(name);
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    let shared = make_shared_data_for_tests();
    // Exercise both missing-nonce directions, plus the legacy None/None pair.
    for (index, (injected, row_nonce, watcher_nonce)) in [
        (false, None, Some("terminal-A")),
        (true, Some("row-B"), None),
        (false, None, None),
    ]
    .into_iter()
    .enumerate()
    {
        let channel = serenity::ChannelId::new(
            57_550_000 + u64::from(std::process::id()) * 10 + index as u64,
        );
        let session = format!("id0-fixture-{}", channel.get());
        let output = root.join(format!("output-{index}.jsonl"));
        std::fs::write(&output, "fixture").unwrap();
        let a = handle(&session, output.to_str().unwrap());
        let start = if newer { 50 + index as u64 } else { 10 };
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            channel.get(),
            None,
            0,
            0,
            0,
            "external input".into(),
            None,
            Some(session.clone()),
            Some(a.output_path.clone()),
            None,
            start,
        );
        row.turn_source = if injected {
            inflight::TurnSource::Managed
        } else {
            inflight::TurnSource::ExternalInput
        };
        row.injected_prompt_message_id = injected.then_some(777);
        row.turn_nonce = row_nonce.map(str::to_owned);
        inflight::save_inflight_state(&row).unwrap();
        let late_row = inflight::load_inflight_state(&ProviderKind::Claude, channel.get());
        let loaded = late_row.as_ref().expect("fixture row was really saved");
        assert_eq!(loaded.turn_nonce.as_deref(), row_nonce);
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            channel.get(),
        );
        assert!(path.starts_with(&root));
        // Saving and compatibility loading can stamp metadata; pin the actual late row.
        let before = std::fs::read(&path).unwrap();
        let completion_stale =
            committed_completion_is_stale_for_newer_turn(None, late_row.as_ref(), &session, 50);
        let anchor_stale =
            committed_anchor_cleanup_is_stale_for_newer_turn(None, late_row.as_ref(), &session, 50);
        assert_eq!((completion_stale, anchor_stale), (false, newer));
        assert!(tui_direct_abort_marker::load_all().is_empty());
        assert!(
            tui_direct_abort_marker::load_commit_tombstones("claude", channel.get()).is_empty()
        );
        let watcher_nonce = watcher_nonce.map(str::to_owned);
        let context = TerminalCommitEpilogueContext {
            shared: &shared,
            channel_id: channel,
            watcher_provider: &ProviderKind::Claude,
            provider_kind: &ProviderKind::Claude,
            tmux_session_name: &a.tmux_session_name,
            output_path: &a.output_path,
            relay_coord: &Arc::new(TmuxRelayCoord::new(channel)),
            turn_delivered: &a.turn_delivered,
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            run_terminal_commit_epilogue(
                &context,
                TerminalCommitEpilogueLocals {
                    terminal_output_committed: true,
                    lifecycle_stage_paused: false,
                    relay_suppressed: false,
                    has_assistant_response: false,
                    completion_is_stale_for_newer_turn: completion_stale,
                    anchor_cleanup_is_stale_for_newer_turn: anchor_stale,
                    inflight_state: &late_row,
                    inflight_before_relay: &None,
                    full_response: &String::new(),
                    watcher_turn_nonce: &watcher_nonce,
                    resolved_did: &None,
                    dispatch_ok: false,
                    terminal_delivery_committed: true,
                    watcher_tui_gate_outcome: TuiCompletionGateOutcome::NotGated,
                    // This also exercises the changed tombstone guard for id-0 rows.
                    tui_direct_anchor_terminal_body_visible: true,
                    terminal_kind: None,
                    terminal_evidence_offset: Some(50),
                    finish_mailbox_on_completion: true,
                    pre_panel_release_drove_finalize: false,
                    current_offset: 50,
                    data_start_offset: 0,
                },
                &mut TerminalCommitEpilogueState {
                    turn_result_relayed: &mut false,
                    watcher_direct_terminal_idle_committed: &mut true,
                    monitor_auto_turn_claimed: &mut false,
                    monitor_auto_turn_finished: &mut false,
                    monitor_auto_turn_synthetic_msg_id: &mut None,
                    monitor_auto_turn_ledger_generation: &mut None,
                },
            ),
        )
        .await
        .expect("actual epilogue completes within fixture timeout");
        assert_eq!(outcome, TerminalCommitEpilogueOutcome::Fallthrough);
        if newer {
            assert_eq!(
                std::fs::read(&path).ok().as_deref(),
                Some(before.as_slice()),
                "older terminal must preserve the newer zero-id row bytes",
            );
        } else {
            assert!(!path.exists(), "current zero-id row must still be cleared");
        }
        let tombstones = tui_direct_abort_marker::load_commit_tombstones("claude", channel.get());
        assert_eq!(
            tombstones.len(),
            usize::from(!newer),
            "only the current row may be tombstoned"
        );
        assert!(!root.join("probes").exists(), "no tmux probe was needed");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn newer_zero_id_row_survives_old_terminal() {
    exercise_zero_id_cleanup("newer_zero_id_row_survives_old_terminal", true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn current_zero_id_row_can_complete() {
    exercise_zero_id_cleanup("current_zero_id_row_can_complete", false).await;
}
