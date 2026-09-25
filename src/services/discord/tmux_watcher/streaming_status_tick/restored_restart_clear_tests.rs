//! A drained Claude TUI session-bound row restored by the next process runs loader → yield guard
//! → soft terminal → Discord POST → watcher commit wrapper → terminal-commit epilogue clear.
use super::*;
use crate::services::discord::tmux::tmux_watcher::{commit_decisions, turn_identity};

const SESSION: &str = "AgentDesk-claude-restored-6210";
const PRIOR: &str =
    "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"older\"}]}}\n";
const TURN: &str = concat!(
    "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"restored final\"}]},\"sessionId\":\"s6210\"}\n",
    "{\"type\":\"system\",\"subtype\":\"stop_hook_summary\",\"sessionId\":\"s6210\",\"hookCount\":1,\"hasOutput\":true}\n",
);

async fn run_epilogue(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    output: &str,
    before: &Option<InflightTurnState>,
    after: &Option<InflightTurnState>,
    body: &String,
    (data_start_offset, current_offset): (u64, u64),
) {
    let tmux = SESSION.to_string();
    let output = output.to_string();
    let nonce = turn_identity::matching_watcher_turn_nonce(after.as_ref(), SESSION);
    assert!(nonce.is_some(), "the restored row carries its turn nonce");
    let completion_is_stale_for_newer_turn = committed_completion_is_stale_for_newer_turn(
        before.as_ref(),
        after.as_ref(),
        &tmux,
        current_offset,
    );
    let anchor_cleanup_is_stale_for_newer_turn = committed_anchor_cleanup_is_stale_for_newer_turn(
        before.as_ref(),
        after.as_ref(),
        &tmux,
        current_offset,
    );
    assert!(!completion_is_stale_for_newer_turn && !anchor_cleanup_is_stale_for_newer_turn);
    let context = TerminalCommitEpilogueContext {
        shared,
        channel_id: channel,
        watcher_provider: &ProviderKind::Claude,
        provider_kind: &ProviderKind::Claude,
        tmux_session_name: &tmux,
        output_path: &output,
        relay_coord: &Arc::new(crate::services::discord::TmuxRelayCoord::new(channel)),
        turn_delivered: &Arc::new(AtomicBool::new(false)),
    };
    run_terminal_commit_epilogue(
        &context,
        TerminalCommitEpilogueLocals {
            terminal_output_committed: true,
            lifecycle_stage_paused: false,
            relay_suppressed: false,
            has_assistant_response: true,
            completion_is_stale_for_newer_turn,
            anchor_cleanup_is_stale_for_newer_turn,
            inflight_state: after,
            inflight_before_relay: before,
            full_response: body,
            watcher_turn_nonce: &nonce,
            resolved_did: &None,
            dispatch_ok: true,
            terminal_delivery_committed: true,
            watcher_tui_gate_outcome: TuiCompletionGateOutcome::NotGated,
            tui_direct_anchor_terminal_body_visible: false,
            terminal_kind: Some(WatcherTerminalKind::SoftStopHookSummary),
            terminal_evidence_offset: Some(current_offset.saturating_sub(1)),
            finish_mailbox_on_completion: false,
            pre_panel_release_drove_finalize: false,
            current_offset,
            data_start_offset,
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
    .await;
}

/// The recovery watcher delivers, commits and clears the drained row the next process restored.
#[test]
fn restored_restart_row_delivered_committed_and_cleared_through_entry_points() {
    let (_lock, guard) = isolate_root();
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let generation = 62_100;
    let channel = ChannelId::new(6_210_000_777);
    let output = guard.root.path().join("restored.jsonl");
    std::fs::write(&output, format!("{PRIOR}{TURN}")).expect("transcript");
    let generation_file = crate::services::tmux_common::session_temp_path(SESSION, "generation");
    std::fs::write(&generation_file, b"1").expect("generation file");
    let (turn_start, current) = (PRIOR.len() as u64, (PRIOR.len() + TURN.len()) as u64);

    // The outgoing process authors the row and drains with it in flight.
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(generation));
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel.get(),
        None,
        1,
        6_210_001,
        0,
        "restored prompt".to_string(),
        Some("s6210".to_string()),
        Some(SESSION.to_string()),
        Some(output.to_string_lossy().into_owned()),
        None,
        turn_start,
    );
    row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    row.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::SessionBoundRelay);
    row.set_restart_mode(crate::services::discord::InflightRestartMode::DrainRestart);
    save_inflight_state(&row).expect("drained row");

    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(generation + 1));
    let restored = crate::services::discord::inflight::load_inflight_states(&ProviderKind::Claude)
        .into_iter()
        .find(|state| state.channel_id == channel.get())
        .expect("the loader keeps the replacement-window row");
    assert!(restored.restored_claude_session_bound_restart());
    assert!(
        !crate::services::discord::tmux::watcher_should_yield_to_active_bridge_turn(
            &ProviderKind::Claude,
            channel,
            SESSION,
            turn_start,
            current,
        )
    );

    let mut buffer = TURN.to_string();
    let (mut lines, mut tools) = (
        crate::services::session_backend::StreamLineState::new(),
        WatcherToolState::new(),
    );
    let mut body = String::new();
    let outcome =
        crate::services::discord::tmux::tmux_output_stream::process_watcher_lines_for_turn(
            &mut buffer,
            &mut lines,
            &mut body,
            &mut tools,
            Some(turn_start),
            restored.turn_start_offset,
        );
    assert!(outcome.soft_terminal_candidate && !outcome.found_result);
    assert_eq!(body, "restored final");

    let fx = Fixture {
        provider: ProviderKind::Claude,
        channel,
        tmux: SESSION.to_string(),
        output_path: output.to_string_lossy().into_owned(),
        identity: InflightTurnIdentity::from_state(&restored),
    };
    let row_path = fx.path();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let rec = recorder(channel, true).await;
            crate::services::discord::http::send_channel_message(&rec.http, channel, &body)
                .await
                .expect("terminal POST");
            assert_eq!(rec.seen("POST").len(), 1, "one terminal delivery");
            assert_eq!(rec.bodies.lock().unwrap().as_slice(), [body.clone()]);

            let before = Some(restored.clone());
            let identity = turn_identity::matching_watcher_turn_identity(before.as_ref(), SESSION);
            assert!(commit_decisions::mark_watcher_terminal_delivery_committed(
                &ProviderKind::Claude,
                channel,
                SESSION,
                identity.as_ref(),
                &body,
                turn_start,
                None,
                current,
            ));
            let after = load_inflight_state(&ProviderKind::Claude, channel.get());
            assert!(
                after
                    .as_ref()
                    .is_some_and(|s| s.terminal_delivery_committed)
            );
            let shared = crate::services::discord::make_shared_data_for_tests();
            let output_path = fx.output_path.clone();
            run_epilogue(
                &shared,
                channel,
                &output_path,
                &before,
                &after,
                &body,
                (turn_start, current),
            )
            .await;
            assert_eq!(rec.total(), 1, "the epilogue sends nothing more");
        });
    crate::services::discord::runtime_store::set_process_generation_for_tests(None);
    let _ = std::fs::remove_file(&generation_file);
    assert!(
        !row_path.exists(),
        "the committed restored row is cleared from disk"
    );
}

/// The watcher loop must still commit through the wrapper and hand the committed row to the epilogue.
#[test]
fn restored_restart_watcher_wires_commit_and_epilogue_clear() {
    let watcher: String = include_str!("../../tmux_watcher.rs")
        .split_whitespace()
        .collect();
    assert!(watcher.contains(
        "letterminal_delivery_committed=relay_ok&&has_assistant_response&&mark_watcher_terminal_delivery_committed("
    ));
    assert!(
        watcher.contains("matchrun_terminal_commit_epilogue(&terminal_commit_epilogue_context,")
    );
}
